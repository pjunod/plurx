//! A bounded, Plurx-owned adapter around the reviewed PGS parser.
//!
//! It accepts only raw SUP streams that pass an allocation-free structural
//! preflight. Direct MKV/M2TS parsing remains outside the reviewed boundary:
//! the server demuxes one subtitle stream and this crate owns all PGS state.

use libpgs::pgs::{CompositionState, OdsData, PdsData, SegmentType, SequenceFlag};
use libpgs::{ContainerFormat, Extractor};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

/// The exact parser release whose behavior this adapter was written against.
pub const REVIEWED_LIBPGS_VERSION: &str = "0.6.0";

const PGS_HEADER_BYTES: usize = 13;
const PGS_MAGIC: [u8; 2] = [0x50, 0x47];

/// Review limits for the feasibility harness.
///
/// These are not a production compatibility promise. Representative-media
/// measurements must justify any changes before a server route is added.
#[derive(Debug, Clone)]
pub struct ParserLimits {
    pub max_sup_bytes: u64,
    pub max_display_sets: u64,
    pub max_segments_per_display_set: usize,
    pub max_payload_bytes_per_display_set: usize,
    pub max_canvas_width: u16,
    pub max_canvas_height: u16,
    pub max_canvas_pixels: usize,
    pub max_objects_per_composition: usize,
    pub max_object_rgba_bytes: usize,
    pub max_object_rle_bytes: usize,
    pub max_cached_objects: usize,
    pub max_cached_pixel_bytes: usize,
    pub max_palettes: usize,
    /// Maximum RGBA bytes retained by [`normalize_sup`]. Fingerprint-only
    /// inspection does not allocate this output.
    pub max_normalized_rgba_bytes: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_sup_bytes: 512 * 1024 * 1024,
            max_display_sets: 250_000,
            max_segments_per_display_set: 512,
            max_payload_bytes_per_display_set: 64 * 1024 * 1024,
            max_canvas_width: 4096,
            max_canvas_height: 2160,
            max_canvas_pixels: 8_847_360,
            max_objects_per_composition: 64,
            max_object_rgba_bytes: 36 * 1024 * 1024,
            max_object_rle_bytes: 32 * 1024 * 1024,
            max_cached_objects: 1024,
            max_cached_pixel_bytes: 128 * 1024 * 1024,
            max_palettes: 64,
            max_normalized_rgba_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Stable summary for fixture comparison and representative-media measurement.
#[derive(Debug, Serialize)]
pub struct InspectionReport {
    pub adapter_profile: &'static str,
    pub libpgs_version: &'static str,
    pub source_bytes: u64,
    pub parser_bytes_read: u64,
    pub segments: u64,
    pub display_sets: u64,
    pub content_display_sets: u64,
    pub clear_display_sets: u64,
    pub duplicate_timestamps: u64,
    pub palette_definitions: u64,
    pub object_definitions: u64,
    pub max_canvas_width: u16,
    pub max_canvas_height: u16,
    pub max_canvas_pixels: usize,
    pub max_composition_objects: usize,
    pub max_object_rgba_bytes: usize,
    pub max_object_rle_bytes: usize,
    pub peak_cached_pixel_bytes: usize,
    pub compositions: Vec<CompositionFingerprint>,
}

/// A source-time composition digest without exposing `libpgs` types.
#[derive(Debug, Serialize)]
pub struct CompositionFingerprint {
    pub pts_90khz: u64,
    pub start_ms: f64,
    pub object_count: usize,
    pub sha256: String,
}

/// A complete source-time overlay track with no `libpgs` types exposed.
///
/// Every composition is a snapshot, not a delta. A composition with no
/// objects is an authored clear event.
#[derive(Debug)]
pub struct NormalizedTrack {
    pub report: InspectionReport,
    pub compositions: Vec<NormalizedComposition>,
}

#[derive(Debug)]
pub struct NormalizedComposition {
    pub pts_90khz: u64,
    pub start_ms: f64,
    pub canvas_width: u16,
    pub canvas_height: u16,
    pub objects: Vec<NormalizedObject>,
}

#[derive(Debug)]
pub struct NormalizedObject {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// Row-major, non-premultiplied RGBA8 pixels for the authored crop.
    pub rgba: Vec<u8>,
    /// Stable digest of dimensions and RGBA content, before PNG encoding.
    pub rgba_sha256: String,
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("candidate parser error: {0}")]
    Parser(#[from] libpgs::error::PgsError),
    #[error("malformed PGS: {0}")]
    Malformed(String),
    #[error("PGS safety limit exceeded: {0}")]
    Limit(String),
    #[error("PGS normalization cancelled")]
    Cancelled,
}

#[derive(Debug)]
struct PreflightReport {
    source_bytes: u64,
    segments: u64,
    display_sets: u64,
    duplicate_timestamps: u64,
}

#[derive(Clone)]
struct StoredObject {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

struct PendingObject {
    version: u8,
    width: u16,
    height: u16,
    expected_rle_bytes: usize,
    rle: Vec<u8>,
}

type Palette = [[u8; 4]; 256];

#[derive(Default)]
struct NormalizerState {
    palettes: HashMap<u8, Palette>,
    objects: HashMap<u16, StoredObject>,
    cached_pixel_bytes: usize,
}

/// Inspect and normalize a raw SUP stream through the reviewed safety profile.
///
/// The preflight bounds the parser's otherwise unbounded display-set assembly.
/// The candidate parser is then run with history disabled, and every bitmap is
/// decoded by the strict Plurx normalizer rather than `libpgs::decode_rle`.
pub fn inspect_sup(
    path: impl AsRef<Path>,
    limits: &ParserLimits,
) -> Result<InspectionReport, AdapterError> {
    Ok(process_sup(path.as_ref(), limits, false, None)?.report)
}

/// Normalize a raw SUP stream into complete, bounded RGBA compositions.
pub fn normalize_sup(
    path: impl AsRef<Path>,
    limits: &ParserLimits,
) -> Result<NormalizedTrack, AdapterError> {
    process_sup(path.as_ref(), limits, true, None)
}

/// The server form of [`normalize_sup`], with cooperative cancellation for
/// its hard preparation deadline. The flag is checked during preflight and at
/// every display-set boundary, so dropping an async join handle does not leave
/// an unbounded blocking worker behind.
pub fn normalize_sup_cancellable(
    path: impl AsRef<Path>,
    limits: &ParserLimits,
    cancelled: &AtomicBool,
) -> Result<NormalizedTrack, AdapterError> {
    process_sup(path.as_ref(), limits, true, Some(cancelled))
}

fn process_sup(
    path: &Path,
    limits: &ParserLimits,
    retain_rgba: bool,
    cancelled: Option<&AtomicBool>,
) -> Result<NormalizedTrack, AdapterError> {
    let preflight = preflight_sup(path, limits, cancelled)?;

    let mut extractor = Extractor::open(path)?.with_history(false);
    if extractor.format() != ContainerFormat::Sup {
        return Err(AdapterError::Malformed(
            "the bounded adapter accepts raw SUP only".into(),
        ));
    }

    let mut report = InspectionReport {
        adapter_profile: "bounded-sup-v1",
        libpgs_version: REVIEWED_LIBPGS_VERSION,
        source_bytes: preflight.source_bytes,
        parser_bytes_read: 0,
        segments: preflight.segments,
        display_sets: 0,
        content_display_sets: 0,
        clear_display_sets: 0,
        duplicate_timestamps: preflight.duplicate_timestamps,
        palette_definitions: 0,
        object_definitions: 0,
        max_canvas_width: 0,
        max_canvas_height: 0,
        max_canvas_pixels: 0,
        max_composition_objects: 0,
        max_object_rgba_bytes: 0,
        max_object_rle_bytes: 0,
        peak_cached_pixel_bytes: 0,
        compositions: Vec::new(),
    };
    let mut state = NormalizerState::default();
    let mut compositions = Vec::new();
    let mut normalized_rgba_bytes = 0usize;

    for parsed in extractor.by_ref() {
        check_cancelled(cancelled)?;
        let track = parsed?;
        normalize_display_set(
            &track.display_set,
            limits,
            &mut state,
            &mut report,
            retain_rgba.then_some(&mut compositions),
            &mut normalized_rgba_bytes,
            cancelled,
        )?;
    }
    report.parser_bytes_read = extractor.stats().bytes_read;

    if report.display_sets != preflight.display_sets {
        return Err(AdapterError::Malformed(format!(
            "preflight found {} display sets but the candidate parser yielded {}",
            preflight.display_sets, report.display_sets
        )));
    }
    if std::fs::metadata(path)?.len() != preflight.source_bytes {
        return Err(AdapterError::Malformed(
            "SUP source changed while it was being inspected".into(),
        ));
    }

    Ok(NormalizedTrack {
        report,
        compositions,
    })
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<(), AdapterError> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err(AdapterError::Cancelled)
    } else {
        Ok(())
    }
}

fn preflight_sup(
    path: &Path,
    limits: &ParserLimits,
    cancelled: Option<&AtomicBool>,
) -> Result<PreflightReport, AdapterError> {
    let source_bytes = std::fs::metadata(path)?.len();
    if source_bytes > limits.max_sup_bytes {
        return Err(AdapterError::Limit(format!(
            "SUP size {source_bytes} exceeds {} bytes",
            limits.max_sup_bytes
        )));
    }

    let mut reader = BufReader::new(File::open(path)?);
    let mut header = [0u8; PGS_HEADER_BYTES];
    let mut segments = 0u64;
    let mut display_sets = 0u64;
    let mut in_display_set = false;
    let mut set_segments = 0usize;
    let mut set_payload_bytes = 0usize;
    let mut last_pcs_pts = None;
    let mut duplicate_timestamps = 0u64;

    while read_header_or_eof(&mut reader, &mut header)? {
        check_cancelled(cancelled)?;
        if header[0..2] != PGS_MAGIC {
            return Err(AdapterError::Malformed(format!(
                "bad segment magic at segment {}",
                segments + 1
            )));
        }
        let kind = header[10];
        if !matches!(kind, 0x14 | 0x15 | 0x16 | 0x17 | 0x80) {
            return Err(AdapterError::Malformed(format!(
                "unknown segment type 0x{kind:02x}"
            )));
        }
        let payload_bytes = u16::from_be_bytes([header[11], header[12]]) as usize;
        let mut consumed_payload_bytes = 0usize;

        if kind == 0x16 {
            if payload_bytes < 8 {
                return Err(AdapterError::Malformed(
                    "PCS payload is too short to carry a composition state".into(),
                ));
            }
            let mut prefix = [0u8; 8];
            reader.read_exact(&mut prefix).map_err(|_| {
                AdapterError::Malformed("PCS payload is truncated before its state".into())
            })?;
            consumed_payload_bytes = prefix.len();
            if !matches!(prefix[7], 0x00 | 0x40 | 0x80) {
                return Err(AdapterError::Malformed(format!(
                    "unsupported PCS composition state 0x{:02x}",
                    prefix[7]
                )));
            }
        }

        match kind {
            0x16 => {
                if in_display_set {
                    return Err(AdapterError::Malformed(
                        "a new PCS started before the preceding display set ended".into(),
                    ));
                }
                in_display_set = true;
                set_segments = 0;
                set_payload_bytes = 0;
                let pts = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as u64;
                if let Some(previous) = last_pcs_pts {
                    if pts < previous {
                        return Err(AdapterError::Malformed(format!(
                            "PCS timestamp moved backwards from {previous} to {pts}"
                        )));
                    }
                    if pts == previous {
                        duplicate_timestamps += 1;
                    }
                }
                last_pcs_pts = Some(pts);
            }
            0x80 if !in_display_set => {
                return Err(AdapterError::Malformed(
                    "END appeared without an open display set".into(),
                ));
            }
            _ if !in_display_set => {
                return Err(AdapterError::Malformed(
                    "a PGS display set did not begin with PCS".into(),
                ));
            }
            _ => {}
        }

        segments = segments
            .checked_add(1)
            .ok_or_else(|| AdapterError::Limit("segment counter overflowed".into()))?;
        set_segments = set_segments
            .checked_add(1)
            .ok_or_else(|| AdapterError::Limit("display-set segment counter overflowed".into()))?;
        set_payload_bytes = set_payload_bytes
            .checked_add(payload_bytes)
            .ok_or_else(|| AdapterError::Limit("display-set byte counter overflowed".into()))?;
        if set_segments > limits.max_segments_per_display_set {
            return Err(AdapterError::Limit(format!(
                "display set has more than {} segments",
                limits.max_segments_per_display_set
            )));
        }
        if set_payload_bytes > limits.max_payload_bytes_per_display_set {
            return Err(AdapterError::Limit(format!(
                "display set has more than {} payload bytes",
                limits.max_payload_bytes_per_display_set
            )));
        }
        discard_exact(&mut reader, payload_bytes - consumed_payload_bytes)?;

        if kind == 0x80 {
            display_sets += 1;
            if display_sets > limits.max_display_sets {
                return Err(AdapterError::Limit(format!(
                    "track has more than {} display sets",
                    limits.max_display_sets
                )));
            }
            in_display_set = false;
        }
    }

    if in_display_set {
        return Err(AdapterError::Malformed(
            "SUP ended before the final display set END segment".into(),
        ));
    }
    if display_sets == 0 {
        return Err(AdapterError::Malformed(
            "SUP contains no complete display sets".into(),
        ));
    }

    Ok(PreflightReport {
        source_bytes,
        segments,
        display_sets,
        duplicate_timestamps,
    })
}

fn read_header_or_eof(
    reader: &mut impl Read,
    header: &mut [u8; PGS_HEADER_BYTES],
) -> Result<bool, AdapterError> {
    let first = reader.read(&mut header[..1])?;
    if first == 0 {
        return Ok(false);
    }
    reader.read_exact(&mut header[1..]).map_err(|error| {
        AdapterError::Malformed(format!("truncated PGS segment header: {error}"))
    })?;
    Ok(true)
}

fn discard_exact(reader: &mut impl Read, mut bytes: usize) -> Result<(), AdapterError> {
    let mut scratch = [0u8; 8192];
    while bytes > 0 {
        let wanted = bytes.min(scratch.len());
        let read = reader.read(&mut scratch[..wanted])?;
        if read == 0 {
            return Err(AdapterError::Malformed(
                "segment payload is truncated".into(),
            ));
        }
        bytes -= read;
    }
    Ok(())
}

fn normalize_display_set(
    display_set: &libpgs::pgs::DisplaySet,
    limits: &ParserLimits,
    state: &mut NormalizerState,
    report: &mut InspectionReport,
    output: Option<&mut Vec<NormalizedComposition>>,
    normalized_rgba_bytes: &mut usize,
    cancelled: Option<&AtomicBool>,
) -> Result<(), AdapterError> {
    check_cancelled(cancelled)?;
    let Some(first) = display_set.segments.first() else {
        return Err(AdapterError::Malformed("empty display set".into()));
    };
    let Some(last) = display_set.segments.last() else {
        return Err(AdapterError::Malformed("empty display set".into()));
    };
    if first.segment_type != SegmentType::PresentationComposition
        || last.segment_type != SegmentType::EndOfDisplaySet
    {
        return Err(AdapterError::Malformed(
            "display set must run from PCS through END".into(),
        ));
    }
    let pcs = first
        .parse_pcs()
        .ok_or_else(|| AdapterError::Malformed("PCS payload is malformed".into()))?;
    let expected_pcs_bytes = 11
        + pcs
            .objects
            .iter()
            .map(|object| if object.crop.is_some() { 16 } else { 8 })
            .sum::<usize>();
    if first.payload.len() != expected_pcs_bytes {
        return Err(AdapterError::Malformed(
            "PCS payload contains truncated or trailing object data".into(),
        ));
    }

    let canvas_pixels = checked_pixels(pcs.video_width, pcs.video_height)?;
    if pcs.video_width == 0
        || pcs.video_height == 0
        || pcs.video_width > limits.max_canvas_width
        || pcs.video_height > limits.max_canvas_height
        || canvas_pixels > limits.max_canvas_pixels
    {
        return Err(AdapterError::Limit(format!(
            "canvas {}x{} exceeds the reviewed bounds",
            pcs.video_width, pcs.video_height
        )));
    }
    if pcs.objects.len() > limits.max_objects_per_composition {
        return Err(AdapterError::Limit(format!(
            "composition has {} objects; limit is {}",
            pcs.objects.len(),
            limits.max_objects_per_composition
        )));
    }

    if !matches!(display_set.composition_state, CompositionState::Normal) {
        state.palettes.clear();
        state.objects.clear();
        state.cached_pixel_bytes = 0;
    }

    let mut pending = HashMap::<u16, PendingObject>::new();
    for segment in display_set
        .segments
        .iter()
        .skip(1)
        .take(display_set.segments.len().saturating_sub(2))
    {
        check_cancelled(cancelled)?;
        match segment.segment_type {
            SegmentType::WindowDefinition => {
                let wds = segment
                    .parse_wds()
                    .ok_or_else(|| AdapterError::Malformed("WDS payload is malformed".into()))?;
                if segment.payload.len() != 1 + wds.windows.len() * 9 {
                    return Err(AdapterError::Malformed(
                        "WDS payload contains trailing window data".into(),
                    ));
                }
                for window in &wds.windows {
                    validate_rect(
                        window.x,
                        window.y,
                        window.width,
                        window.height,
                        pcs.video_width,
                        pcs.video_height,
                        "window",
                    )?;
                }
            }
            SegmentType::PaletteDefinition => {
                let pds = segment
                    .parse_pds()
                    .ok_or_else(|| AdapterError::Malformed("PDS payload is malformed".into()))?;
                apply_palette(state, pds, limits)?;
                report.palette_definitions += 1;
            }
            SegmentType::ObjectDefinition => {
                let ods = segment
                    .parse_ods()
                    .ok_or_else(|| AdapterError::Malformed("ODS payload is malformed".into()))?;
                apply_object_fragment(state, &mut pending, ods, limits, report)?;
                report.object_definitions += 1;
            }
            SegmentType::PresentationComposition | SegmentType::EndOfDisplaySet => {
                return Err(AdapterError::Malformed(
                    "unexpected PCS or END inside a display set".into(),
                ));
            }
        }
    }
    if !pending.is_empty() {
        return Err(AdapterError::Malformed(
            "display set ended with an incomplete ODS object".into(),
        ));
    }

    let mut composition_hasher = Sha256::new();
    composition_hasher.update(b"plurx-pgs-composition-v1");
    composition_hasher.update(pcs.video_width.to_be_bytes());
    composition_hasher.update(pcs.video_height.to_be_bytes());

    let mut normalized_objects = output
        .as_ref()
        .map(|_| Vec::with_capacity(pcs.objects.len()));
    for placement in &pcs.objects {
        check_cancelled(cancelled)?;
        let object = state.objects.get(&placement.object_id).ok_or_else(|| {
            AdapterError::Malformed(format!(
                "composition references missing object {}",
                placement.object_id
            ))
        })?;
        let palette = state.palettes.get(&pcs.palette_id).ok_or_else(|| {
            AdapterError::Malformed(format!(
                "composition references missing palette {}",
                pcs.palette_id
            ))
        })?;

        let (crop_x, crop_y, width, height) = match &placement.crop {
            Some(crop) => {
                validate_rect(
                    crop.x,
                    crop.y,
                    crop.width,
                    crop.height,
                    object.width,
                    object.height,
                    "object crop",
                )?;
                (crop.x, crop.y, crop.width, crop.height)
            }
            None => (0, 0, object.width, object.height),
        };
        validate_rect(
            placement.x,
            placement.y,
            width,
            height,
            pcs.video_width,
            pcs.video_height,
            "composition object",
        )?;

        let (image_hash, rgba) = if normalized_objects.is_some() {
            let rgba = rgba_crop(object, palette, crop_x, crop_y, width, height);
            *normalized_rgba_bytes =
                normalized_rgba_bytes
                    .checked_add(rgba.len())
                    .ok_or_else(|| {
                        AdapterError::Limit("normalized RGBA byte count overflowed".into())
                    })?;
            if *normalized_rgba_bytes > limits.max_normalized_rgba_bytes {
                return Err(AdapterError::Limit(format!(
                    "normalized RGBA output exceeds {} bytes",
                    limits.max_normalized_rgba_bytes
                )));
            }
            (hash_rgba(width, height, &rgba), Some(rgba))
        } else {
            (
                hash_rgba_crop(object, palette, crop_x, crop_y, width, height),
                None,
            )
        };
        composition_hasher.update(placement.object_id.to_be_bytes());
        composition_hasher.update(placement.x.to_be_bytes());
        composition_hasher.update(placement.y.to_be_bytes());
        composition_hasher.update(width.to_be_bytes());
        composition_hasher.update(height.to_be_bytes());
        composition_hasher.update(image_hash);
        if let (Some(objects), Some(rgba)) = (&mut normalized_objects, rgba) {
            objects.push(NormalizedObject {
                x: placement.x,
                y: placement.y,
                width,
                height,
                rgba,
                rgba_sha256: hex::encode(image_hash),
            });
        }
    }

    let object_count = pcs.objects.len();
    if object_count == 0 {
        report.clear_display_sets += 1;
    } else {
        report.content_display_sets += 1;
    }
    report.display_sets += 1;
    report.max_canvas_width = report.max_canvas_width.max(pcs.video_width);
    report.max_canvas_height = report.max_canvas_height.max(pcs.video_height);
    report.max_canvas_pixels = report.max_canvas_pixels.max(canvas_pixels);
    report.max_composition_objects = report.max_composition_objects.max(object_count);
    report.peak_cached_pixel_bytes = report.peak_cached_pixel_bytes.max(state.cached_pixel_bytes);
    report.compositions.push(CompositionFingerprint {
        pts_90khz: display_set.pts,
        start_ms: display_set.pts as f64 / 90.0,
        object_count,
        sha256: hex::encode(composition_hasher.finalize()),
    });
    if let (Some(output), Some(objects)) = (output, normalized_objects) {
        output.push(NormalizedComposition {
            pts_90khz: display_set.pts,
            start_ms: display_set.pts as f64 / 90.0,
            canvas_width: pcs.video_width,
            canvas_height: pcs.video_height,
            objects,
        });
    }

    Ok(())
}

fn apply_palette(
    state: &mut NormalizerState,
    pds: PdsData,
    limits: &ParserLimits,
) -> Result<(), AdapterError> {
    if !state.palettes.contains_key(&pds.id) && state.palettes.len() >= limits.max_palettes {
        return Err(AdapterError::Limit(format!(
            "more than {} palettes retained in one epoch",
            limits.max_palettes
        )));
    }
    let palette = state.palettes.entry(pds.id).or_insert([[0; 4]; 256]);
    for entry in pds.entries {
        palette[entry.id as usize] =
            ycrcb_to_rgba(entry.luminance, entry.cr, entry.cb, entry.alpha);
    }
    Ok(())
}

fn apply_object_fragment(
    state: &mut NormalizerState,
    pending: &mut HashMap<u16, PendingObject>,
    ods: OdsData,
    limits: &ParserLimits,
    report: &mut InspectionReport,
) -> Result<(), AdapterError> {
    match ods.sequence {
        SequenceFlag::Complete => {
            let (width, height, expected) = first_fragment_shape(&ods, limits)?;
            if ods.rle_data.len() != expected {
                return Err(AdapterError::Malformed(format!(
                    "complete object {} declares {expected} RLE bytes but carries {}",
                    ods.id,
                    ods.rle_data.len()
                )));
            }
            store_object(state, ods.id, width, height, ods.rle_data, limits, report)
        }
        SequenceFlag::First => {
            let (width, height, expected_rle_bytes) = first_fragment_shape(&ods, limits)?;
            if pending.contains_key(&ods.id) {
                return Err(AdapterError::Malformed(format!(
                    "object {} has two unfinished first fragments",
                    ods.id
                )));
            }
            if ods.rle_data.len() > expected_rle_bytes {
                return Err(AdapterError::Malformed(format!(
                    "object {} first fragment exceeds its declared length",
                    ods.id
                )));
            }
            pending.insert(
                ods.id,
                PendingObject {
                    version: ods.version,
                    width,
                    height,
                    expected_rle_bytes,
                    rle: ods.rle_data,
                },
            );
            Ok(())
        }
        SequenceFlag::Continuation | SequenceFlag::Last => {
            let Some(object) = pending.get_mut(&ods.id) else {
                return Err(AdapterError::Malformed(format!(
                    "object {} continuation has no first fragment",
                    ods.id
                )));
            };
            if object.version != ods.version {
                return Err(AdapterError::Malformed(format!(
                    "object {} changed version between fragments",
                    ods.id
                )));
            }
            let new_len = object
                .rle
                .len()
                .checked_add(ods.rle_data.len())
                .ok_or_else(|| AdapterError::Limit("object RLE length overflowed".into()))?;
            if new_len > object.expected_rle_bytes || new_len > limits.max_object_rle_bytes {
                return Err(AdapterError::Limit(format!(
                    "object {} exceeds its bounded RLE length",
                    ods.id
                )));
            }
            object.rle.extend_from_slice(&ods.rle_data);
            if ods.sequence == SequenceFlag::Last {
                let object = pending
                    .remove(&ods.id)
                    .ok_or_else(|| AdapterError::Malformed("pending object disappeared".into()))?;
                if object.rle.len() != object.expected_rle_bytes {
                    return Err(AdapterError::Malformed(format!(
                        "object {} ended at {} RLE bytes; declared {}",
                        ods.id,
                        object.rle.len(),
                        object.expected_rle_bytes
                    )));
                }
                store_object(
                    state,
                    ods.id,
                    object.width,
                    object.height,
                    object.rle,
                    limits,
                    report,
                )?;
            }
            Ok(())
        }
    }
}

fn first_fragment_shape(
    ods: &OdsData,
    limits: &ParserLimits,
) -> Result<(u16, u16, usize), AdapterError> {
    let width = ods
        .width
        .ok_or_else(|| AdapterError::Malformed("first ODS fragment has no width".into()))?;
    let height = ods
        .height
        .ok_or_else(|| AdapterError::Malformed("first ODS fragment has no height".into()))?;
    if ods.data_length < 4 {
        return Err(AdapterError::Malformed(format!(
            "object {} declares an ODS length smaller than its dimensions",
            ods.id
        )));
    }
    let pixels = checked_pixels(width, height)?;
    let rgba_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| AdapterError::Limit("object RGBA length overflowed".into()))?;
    if width == 0 || height == 0 || rgba_bytes > limits.max_object_rgba_bytes {
        return Err(AdapterError::Limit(format!(
            "object {} dimensions {}x{} require {rgba_bytes} RGBA bytes",
            ods.id, width, height
        )));
    }
    let rle_bytes = (ods.data_length - 4) as usize;
    if rle_bytes > limits.max_object_rle_bytes {
        return Err(AdapterError::Limit(format!(
            "object {} declares {rle_bytes} RLE bytes; limit is {}",
            ods.id, limits.max_object_rle_bytes
        )));
    }
    Ok((width, height, rle_bytes))
}

fn store_object(
    state: &mut NormalizerState,
    id: u16,
    width: u16,
    height: u16,
    rle: Vec<u8>,
    limits: &ParserLimits,
    report: &mut InspectionReport,
) -> Result<(), AdapterError> {
    let pixels = decode_rle_strict(&rle, width, height)?;
    let rgba_bytes = pixels
        .len()
        .checked_mul(4)
        .ok_or_else(|| AdapterError::Limit("object RGBA length overflowed".into()))?;
    if rgba_bytes > limits.max_object_rgba_bytes {
        return Err(AdapterError::Limit(format!(
            "object {id} expands to {rgba_bytes} RGBA bytes"
        )));
    }

    let previous_bytes = state
        .objects
        .get(&id)
        .map_or(0, |object| object.pixels.len());
    let next_cached_bytes = state
        .cached_pixel_bytes
        .checked_sub(previous_bytes)
        .and_then(|bytes| bytes.checked_add(pixels.len()))
        .ok_or_else(|| AdapterError::Limit("cached object bytes overflowed".into()))?;
    let next_object_count = state.objects.len() + usize::from(!state.objects.contains_key(&id));
    if next_object_count > limits.max_cached_objects {
        return Err(AdapterError::Limit(format!(
            "more than {} objects retained in one epoch",
            limits.max_cached_objects
        )));
    }
    if next_cached_bytes > limits.max_cached_pixel_bytes {
        return Err(AdapterError::Limit(format!(
            "retained object pixels exceed {} bytes",
            limits.max_cached_pixel_bytes
        )));
    }

    report.max_object_rgba_bytes = report.max_object_rgba_bytes.max(rgba_bytes);
    report.max_object_rle_bytes = report.max_object_rle_bytes.max(rle.len());
    state.cached_pixel_bytes = next_cached_bytes;
    state.objects.insert(
        id,
        StoredObject {
            width,
            height,
            pixels,
        },
    );
    Ok(())
}

fn decode_rle_strict(rle: &[u8], width: u16, height: u16) -> Result<Vec<u8>, AdapterError> {
    let width = width as usize;
    let height = height as usize;
    let total = width
        .checked_mul(height)
        .ok_or_else(|| AdapterError::Limit("object pixel count overflowed".into()))?;
    let mut pixels = Vec::with_capacity(total);
    let mut offset = 0usize;
    let mut row = 0usize;
    let mut column = 0usize;

    while offset < rle.len() {
        if row >= height {
            return Err(AdapterError::Malformed(
                "RLE contains data after the final row".into(),
            ));
        }
        let first = rle[offset];
        offset += 1;
        if first != 0 {
            push_run(&mut pixels, &mut column, width, 1, first)?;
            continue;
        }
        let flag = *rle
            .get(offset)
            .ok_or_else(|| AdapterError::Malformed("RLE escape is truncated".into()))?;
        offset += 1;
        if flag == 0 {
            pixels.resize(pixels.len() + (width - column), 0);
            column = 0;
            row += 1;
            continue;
        }

        let (run, color) = match flag & 0xc0 {
            0x00 => ((flag & 0x3f) as usize, 0),
            0x40 => {
                let low = *rle.get(offset).ok_or_else(|| {
                    AdapterError::Malformed("long transparent RLE run is truncated".into())
                })?;
                offset += 1;
                ((((flag & 0x3f) as usize) << 8) | low as usize, 0)
            }
            0x80 => {
                let color = *rle.get(offset).ok_or_else(|| {
                    AdapterError::Malformed("colored RLE run is truncated".into())
                })?;
                offset += 1;
                ((flag & 0x3f) as usize, color)
            }
            0xc0 => {
                let low = *rle.get(offset).ok_or_else(|| {
                    AdapterError::Malformed("long colored RLE run is truncated".into())
                })?;
                let color = *rle.get(offset + 1).ok_or_else(|| {
                    AdapterError::Malformed("long colored RLE color is truncated".into())
                })?;
                offset += 2;
                ((((flag & 0x3f) as usize) << 8) | low as usize, color)
            }
            _ => unreachable!("top two bits cover all RLE run forms"),
        };
        if run == 0 {
            return Err(AdapterError::Malformed(
                "RLE contains a zero-length run".into(),
            ));
        }
        push_run(&mut pixels, &mut column, width, run, color)?;
    }

    if row + 1 == height && column == width {
        row += 1;
        column = 0;
    }
    if row != height || column != 0 || pixels.len() != total {
        return Err(AdapterError::Malformed(format!(
            "RLE ended at row {row}, column {column}; expected {height} rows of {width} pixels"
        )));
    }
    Ok(pixels)
}

fn push_run(
    pixels: &mut Vec<u8>,
    column: &mut usize,
    width: usize,
    run: usize,
    color: u8,
) -> Result<(), AdapterError> {
    let next_column = column
        .checked_add(run)
        .ok_or_else(|| AdapterError::Limit("RLE column overflowed".into()))?;
    if next_column > width {
        return Err(AdapterError::Malformed(format!(
            "RLE run crosses a row boundary ({column} + {run} > {width})"
        )));
    }
    pixels.resize(pixels.len() + run, color);
    *column = next_column;
    Ok(())
}

fn checked_pixels(width: u16, height: u16) -> Result<usize, AdapterError> {
    (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| AdapterError::Limit("pixel count overflowed".into()))
}

fn validate_rect(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    bounds_width: u16,
    bounds_height: u16,
    label: &str,
) -> Result<(), AdapterError> {
    if width == 0
        || height == 0
        || u32::from(x) + u32::from(width) > u32::from(bounds_width)
        || u32::from(y) + u32::from(height) > u32::from(bounds_height)
    {
        return Err(AdapterError::Malformed(format!(
            "{label} ({x},{y} {width}x{height}) exceeds {bounds_width}x{bounds_height}"
        )));
    }
    Ok(())
}

fn hash_rgba_crop(
    object: &StoredObject,
    palette: &Palette,
    crop_x: u16,
    crop_y: u16,
    width: u16,
    height: u16,
) -> [u8; 32] {
    let rgba = rgba_crop(object, palette, crop_x, crop_y, width, height);
    hash_rgba(width, height, &rgba)
}

fn hash_rgba(width: u16, height: u16, rgba: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"plurx-pgs-rgba-v1");
    hasher.update(width.to_be_bytes());
    hasher.update(height.to_be_bytes());
    hasher.update(rgba);
    hasher.finalize().into()
}

fn rgba_crop(
    object: &StoredObject,
    palette: &Palette,
    crop_x: u16,
    crop_y: u16,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    let stride = object.width as usize;
    for y in crop_y as usize..(crop_y + height) as usize {
        let start = y * stride + crop_x as usize;
        for index in &object.pixels[start..start + width as usize] {
            rgba.extend_from_slice(&palette[*index as usize]);
        }
    }
    rgba
}

fn ycrcb_to_rgba(y: u8, cr: u8, cb: u8, alpha: u8) -> [u8; 4] {
    let c = i32::from(y) - 16;
    let d = i32::from(cb) - 128;
    let e = i32::from(cr) - 128;
    let red = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
    let green = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
    let blue = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
    [red, green, blue, alpha]
}

#[cfg(test)]
mod tests {
    use super::*;
    use libpgs::pgs::{
        encode_rle, CompositionObject, CropInfo, OdsData, PaletteEntry, PcsData, PdsData,
        PgsSegment, SequenceFlag,
    };
    use std::io::Write;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn write_sup(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temporary SUP");
        file.write_all(bytes).expect("write SUP");
        file.flush().expect("flush SUP");
        file
    }

    fn pcs(pts_ms: u64, state: CompositionState, objects: Vec<CompositionObject>) -> PgsSegment {
        PgsSegment::from_pcs(
            pts_ms * 90,
            0,
            &PcsData {
                video_width: 1920,
                video_height: 1080,
                composition_number: pts_ms as u16,
                composition_state: state,
                palette_only: false,
                palette_id: 0,
                objects,
            },
        )
    }

    fn palette(pts_ms: u64, y: u8, cr: u8, cb: u8) -> PgsSegment {
        PgsSegment::from_pds(
            pts_ms * 90,
            0,
            &PdsData {
                id: 0,
                version: 0,
                entries: vec![PaletteEntry {
                    id: 1,
                    luminance: y,
                    cr,
                    cb,
                    alpha: 255,
                }],
            },
        )
    }

    fn object(pts_ms: u64, id: u16, width: u16, height: u16, rle: Vec<u8>) -> PgsSegment {
        PgsSegment::from_ods(
            pts_ms * 90,
            0,
            &OdsData {
                id,
                version: 0,
                sequence: SequenceFlag::Complete,
                data_length: rle.len() as u32 + 4,
                width: Some(width),
                height: Some(height),
                rle_data: rle,
            },
        )
    }

    fn fragmented_object(
        pts_ms: u64,
        id: u16,
        width: u16,
        height: u16,
        rle: &[u8],
        split: usize,
    ) -> [PgsSegment; 2] {
        let mut first = PgsSegment::from_ods(
            pts_ms * 90,
            0,
            &OdsData {
                id,
                version: 0,
                sequence: SequenceFlag::First,
                data_length: rle.len() as u32 + 4,
                width: Some(width),
                height: Some(height),
                rle_data: rle[..split].to_vec(),
            },
        );
        let declared = (rle.len() as u32 + 4).to_be_bytes();
        first.payload[4..7].copy_from_slice(&declared[1..]);
        let last = PgsSegment::from_ods(
            pts_ms * 90,
            0,
            &OdsData {
                id,
                version: 0,
                sequence: SequenceFlag::Last,
                data_length: 0,
                width: None,
                height: None,
                rle_data: rle[split..].to_vec(),
            },
        );
        [first, last]
    }

    fn placement(id: u16) -> CompositionObject {
        CompositionObject {
            object_id: id,
            window_id: 0,
            x: 100,
            y: 100,
            crop: None,
        }
    }

    fn display_set(segments: Vec<PgsSegment>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for segment in segments {
            bytes.extend(segment.to_bytes());
        }
        bytes
    }

    #[test]
    fn normal_show_clear_produces_stable_fingerprints() {
        let rle = encode_rle(&[1, 1, 1, 1], 2, 2).expect("encode fixture");
        let mut bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(7)]),
            palette(1000, 235, 128, 128),
            object(1000, 7, 2, 2, rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        bytes.extend(display_set(vec![
            pcs(2000, CompositionState::Normal, vec![]),
            PgsSegment::end_segment(180_000, 0),
        ]));
        let file = write_sup(&bytes);

        let report = inspect_sup(file.path(), &ParserLimits::default()).expect("valid SUP");
        assert_eq!(report.display_sets, 2);
        assert_eq!(report.content_display_sets, 1);
        assert_eq!(report.clear_display_sets, 1);
        assert_eq!(report.max_object_rgba_bytes, 16);
        assert_eq!(report.compositions.len(), 2);
        assert_eq!(
            report.compositions[0].sha256,
            "da6504d113c368e1c095d0941a8321d09c61bfac9d9c168d37dee585dd6ca501"
        );
        assert_eq!(
            report.compositions[1].sha256,
            "375009aa13c9613c4fa6ca3f7ed7d92a11a1332f907b449d84fc95c7932447d9"
        );
    }

    #[test]
    fn normalized_output_is_complete_rgba_and_preserves_clear_events() {
        let rle = encode_rle(&[1, 1, 1, 1], 2, 2).expect("encode fixture");
        let mut bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(7)]),
            palette(1000, 235, 128, 128),
            object(1000, 7, 2, 2, rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        bytes.extend(display_set(vec![
            pcs(2000, CompositionState::Normal, vec![]),
            PgsSegment::end_segment(180_000, 0),
        ]));
        let file = write_sup(&bytes);

        let track = normalize_sup(file.path(), &ParserLimits::default()).expect("normalize SUP");
        assert_eq!(track.compositions.len(), 2);
        let shown = &track.compositions[0];
        assert_eq!((shown.canvas_width, shown.canvas_height), (1920, 1080));
        assert_eq!((shown.objects[0].x, shown.objects[0].y), (100, 100));
        assert_eq!((shown.objects[0].width, shown.objects[0].height), (2, 2));
        assert_eq!(shown.objects[0].rgba, [255, 255, 255, 255].repeat(4));
        assert!(track.compositions[1].objects.is_empty());
        assert_eq!(
            track.report.compositions[0].sha256,
            inspect_sup(file.path(), &ParserLimits::default())
                .expect("inspect")
                .compositions[0]
                .sha256
        );
    }

    #[test]
    fn normalized_output_obeys_aggregate_rgba_limit() {
        let rle = encode_rle(&[1, 1, 1, 1], 2, 2).expect("encode fixture");
        let bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(7)]),
            palette(1000, 235, 128, 128),
            object(1000, 7, 2, 2, rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        let file = write_sup(&bytes);
        let limits = ParserLimits {
            max_normalized_rgba_bytes: 15,
            ..ParserLimits::default()
        };
        let error = normalize_sup(file.path(), &limits).expect_err("RGBA cap");
        assert!(error
            .to_string()
            .contains("normalized RGBA output exceeds 15 bytes"));
    }

    #[test]
    fn server_normalization_honors_cancellation_before_parsing() {
        let bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![]),
            PgsSegment::end_segment(90_000, 0),
        ]);
        let file = write_sup(&bytes);
        let cancelled = AtomicBool::new(true);
        let error = normalize_sup_cancellable(file.path(), &ParserLimits::default(), &cancelled)
            .expect_err("cancelled normalization");
        assert!(matches!(error, AdapterError::Cancelled));
    }

    #[test]
    fn palette_update_reuses_the_previous_object() {
        let rle = encode_rle(&[1, 1], 2, 1).expect("encode fixture");
        let mut bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(3)]),
            palette(1000, 235, 128, 128),
            object(1000, 3, 2, 1, rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        bytes.extend(display_set(vec![
            pcs(2000, CompositionState::Normal, vec![placement(3)]),
            palette(2000, 81, 240, 90),
            PgsSegment::end_segment(180_000, 0),
        ]));
        let file = write_sup(&bytes);

        let report = inspect_sup(file.path(), &ParserLimits::default()).expect("valid reuse");
        assert_eq!(report.object_definitions, 1);
        assert_eq!(report.palette_definitions, 2);
        assert_ne!(report.compositions[0].sha256, report.compositions[1].sha256);
    }

    #[test]
    fn acquisition_point_does_not_reuse_the_previous_epoch_cache() {
        let rle = encode_rle(&[1, 1], 2, 1).expect("encode fixture");
        let mut bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(3)]),
            palette(1000, 235, 128, 128),
            object(1000, 3, 2, 1, rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        bytes.extend(display_set(vec![
            pcs(2000, CompositionState::AcquisitionPoint, vec![placement(3)]),
            PgsSegment::end_segment(180_000, 0),
        ]));
        let file = write_sup(&bytes);

        let error = inspect_sup(file.path(), &ParserLimits::default())
            .expect_err("acquisition point must carry fresh object state");
        assert!(error.to_string().contains("missing object 3"));
    }

    #[test]
    fn permissive_candidate_rle_is_rejected_by_the_adapter() {
        let bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(1)]),
            palette(1000, 235, 128, 128),
            object(1000, 1, 2, 1, vec![1]),
            PgsSegment::end_segment(90_000, 0),
        ]);
        let file = write_sup(&bytes);

        let error = inspect_sup(file.path(), &ParserLimits::default()).expect_err("short RLE");
        assert!(error.to_string().contains("RLE ended"));
    }

    #[test]
    fn oversized_canvas_is_rejected_before_bitmap_decode() {
        let mut segment = pcs(1000, CompositionState::EpochStart, vec![]);
        segment.payload[0..2].copy_from_slice(&u16::MAX.to_be_bytes());
        segment.payload[2..4].copy_from_slice(&u16::MAX.to_be_bytes());
        let bytes = display_set(vec![segment, PgsSegment::end_segment(90_000, 0)]);
        let file = write_sup(&bytes);

        let error = inspect_sup(file.path(), &ParserLimits::default()).expect_err("huge canvas");
        assert!(error.to_string().contains("canvas 65535x65535"));
    }

    #[test]
    fn truncated_display_set_fails_preflight() {
        let bytes = display_set(vec![pcs(1000, CompositionState::EpochStart, vec![])]);
        let file = write_sup(&bytes);

        let error = inspect_sup(file.path(), &ParserLimits::default()).expect_err("missing END");
        assert!(error.to_string().contains("final display set END"));
    }

    #[test]
    fn duplicate_timestamps_are_measured_but_backwards_time_is_rejected() {
        let mut duplicate = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![]),
            PgsSegment::end_segment(90_000, 0),
        ]);
        duplicate.extend(display_set(vec![
            pcs(1000, CompositionState::Normal, vec![]),
            PgsSegment::end_segment(90_000, 0),
        ]));
        let duplicate_file = write_sup(&duplicate);
        let report = inspect_sup(duplicate_file.path(), &ParserLimits::default())
            .expect("duplicates retain file order");
        assert_eq!(report.duplicate_timestamps, 1);

        let mut backwards = display_set(vec![
            pcs(2000, CompositionState::EpochStart, vec![]),
            PgsSegment::end_segment(180_000, 0),
        ]);
        backwards.extend(display_set(vec![
            pcs(1000, CompositionState::Normal, vec![]),
            PgsSegment::end_segment(90_000, 0),
        ]));
        let backwards_file = write_sup(&backwards);
        let error = inspect_sup(backwards_file.path(), &ParserLimits::default())
            .expect_err("backwards timestamps");
        assert!(error.to_string().contains("moved backwards"));
    }

    #[test]
    fn preflight_bounds_segments_before_candidate_assembly() {
        let bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![]),
            PgsSegment::end_segment(90_000, 0),
        ]);
        let file = write_sup(&bytes);
        let limits = ParserLimits {
            max_segments_per_display_set: 1,
            ..ParserLimits::default()
        };

        let error = inspect_sup(file.path(), &limits).expect_err("segment limit");
        assert!(error.to_string().contains("more than 1 segments"));
    }

    #[test]
    fn fragmented_object_reassembles_before_strict_decode() {
        let rle = encode_rle(&[1, 1, 1, 1, 1, 1, 1, 1], 4, 2).expect("encode fixture");
        let split = rle.len() / 2;
        assert!(split > 0 && split < rle.len());
        let [first, last] = fragmented_object(1000, 9, 4, 2, &rle, split);
        let bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(9)]),
            palette(1000, 235, 128, 128),
            first,
            last,
            PgsSegment::end_segment(90_000, 0),
        ]);
        let file = write_sup(&bytes);

        let report = inspect_sup(file.path(), &ParserLimits::default()).expect("fragmented SUP");
        assert_eq!(report.object_definitions, 2);
        assert_eq!(report.max_object_rgba_bytes, 32);
        assert_eq!(report.content_display_sets, 1);
    }

    #[test]
    fn cropped_composition_hashes_multiple_objects_in_authored_order() {
        let first_rle = encode_rle(&[1, 1, 1, 1, 1, 1, 1, 1], 4, 2).expect("first object");
        let second_rle = encode_rle(&[1, 1, 1, 1], 2, 2).expect("second object");
        let objects = vec![
            CompositionObject {
                object_id: 1,
                window_id: 0,
                x: 100,
                y: 100,
                crop: Some(CropInfo {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
            },
            CompositionObject {
                object_id: 2,
                window_id: 0,
                x: 400,
                y: 200,
                crop: None,
            },
        ];
        let bytes = display_set(vec![
            pcs(1000, CompositionState::EpochStart, objects),
            palette(1000, 235, 128, 128),
            object(1000, 1, 4, 2, first_rle),
            object(1000, 2, 2, 2, second_rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        let file = write_sup(&bytes);

        let report = inspect_sup(file.path(), &ParserLimits::default()).expect("cropped objects");
        assert_eq!(report.max_composition_objects, 2);
        assert_eq!(report.object_definitions, 2);
        assert_eq!(report.compositions[0].object_count, 2);
    }

    #[test]
    fn unsupported_multi_clip_composition_state_is_rejected_explicitly() {
        let mut unsupported = pcs(1000, CompositionState::EpochStart, vec![]);
        unsupported.payload[7] = 0xc0;
        let bytes = display_set(vec![unsupported, PgsSegment::end_segment(90_000, 0)]);
        let file = write_sup(&bytes);

        let error = inspect_sup(file.path(), &ParserLimits::default())
            .expect_err("0xc0 is outside the reviewed candidate model");
        assert!(error
            .to_string()
            .contains("unsupported PCS composition state 0xc0"));
    }

    #[test]
    fn deterministic_mutation_corpus_never_panics() {
        let rle = encode_rle(&[1, 1, 1, 1], 2, 2).expect("encode fixture");
        let base = display_set(vec![
            pcs(1000, CompositionState::EpochStart, vec![placement(7)]),
            palette(1000, 235, 128, 128),
            object(1000, 7, 2, 2, rle),
            PgsSegment::end_segment(90_000, 0),
        ]);
        let limits = ParserLimits {
            max_sup_bytes: 64 * 1024,
            max_display_sets: 32,
            max_segments_per_display_set: 32,
            max_payload_bytes_per_display_set: 64 * 1024,
            max_canvas_width: 1920,
            max_canvas_height: 1080,
            max_canvas_pixels: 2_073_600,
            max_objects_per_composition: 8,
            max_object_rgba_bytes: 1024 * 1024,
            max_object_rle_bytes: 64 * 1024,
            max_cached_objects: 16,
            max_cached_pixel_bytes: 1024 * 1024,
            max_palettes: 8,
            max_normalized_rgba_bytes: 1024 * 1024,
        };
        let mut corpus = Vec::new();
        for end in 0..=base.len() {
            corpus.push(base[..end].to_vec());
        }
        for offset in 0..base.len() {
            for replacement in [0x00, 0x40, 0x80, 0xc0, 0xff] {
                if base[offset] != replacement {
                    let mut mutated = base.clone();
                    mutated[offset] = replacement;
                    corpus.push(mutated);
                }
            }
        }

        for (case, bytes) in corpus.iter().enumerate() {
            let file = write_sup(bytes);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let _ = inspect_sup(file.path(), &limits);
            }));
            assert!(outcome.is_ok(), "mutation case {case} panicked");
        }
    }
}
