//! Fragmented-MP4 surgery for the copy path: read ffmpeg's continuous
//! fragment stream, classify each fragment's opening keyframe, and merge runs
//! of fragments back into single HLS segments.
//!
//! Why this exists. On a `-c:v copy` of an open-GOP source — every disc remux
//! — ffmpeg's HLS muxer cuts a segment at whatever keyframe it finds past the
//! duration floor. Most of those keyframes are CRAs carrying a RASL leading
//! picture, and both Chrome's MSE and Safari's native player treat a segment's
//! first keyframe as a random-access point: the HEVC spec's instruction at
//! random access is to DISCARD the leading pictures, so exactly one frame dies
//! per boundary (`docs/STUTTER-4K.md` §5.3ter — 11/12 boundary-attributed in
//! Safari, 15/23 in Chrome, and zero drops in 1781 frames of the same
//! bitstream played unsegmented). A copy cannot change GOP structure. It can
//! change **where the cuts land**, and that is all this module does: ffmpeg
//! emits one fragment per GOP into a pipe, and plurx publishes a boundary only
//! in front of a fragment whose first frame is a true random-access point.
//!
//! Everything here is pure — bytes in, bytes out, no I/O and no ffmpeg. The
//! session that drives it lives in `plurxd::copyseg`.
//!
//! Three pieces:
//!
//! - [`FragmentReader`] splits a byte feed into [`Init`] (ftyp+moov, which
//!   becomes `init.mp4` verbatim), a [`Fragment`] per GOP, and the trailing
//!   `mfra` index that must never be published.
//! - [`classify`] answers the only question the cut policy asks of a
//!   fragment: is its first frame safe to open a segment with?
//! - [`merge`] concatenates a run of fragments back into one `styp moof mdat`
//!   segment. It moves bytes and rewrites offsets; it never touches sample
//!   data, which is what makes `framemd5` equality with the unsegmented
//!   stream the licence to ship it.

use std::fmt::Write as _;
use std::ops::Range;

/// What went wrong reading someone else's bytes.
///
/// Split two ways on purpose: [`Fmp4Error::Malformed`] means the stream is not
/// what it claims to be, [`Fmp4Error::Unsupported`] means it is legal MP4 that
/// this reader deliberately does not handle (an explicit `base_data_offset`,
/// say — ffmpeg only writes one without `default_base_moof`, which plurx
/// always passes). Both land in the same place at runtime: one warning and a
/// respawn onto the legacy muxer path. Naming them apart is for whoever reads
/// that warning.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Fmp4Error {
    /// The bytes do not parse as the box structure they claim.
    #[error("malformed fMP4: {0}")]
    Malformed(String),
    /// Legal MP4 this reader does not implement.
    #[error("unsupported fMP4: {0}")]
    Unsupported(String),
}

fn malformed<T>(msg: impl Into<String>) -> Result<T, Fmp4Error> {
    Err(Fmp4Error::Malformed(msg.into()))
}

/// What a track carries. Only `Video` is load-bearing — it is the track whose
/// keyframes decide where segments may be cut and whose durations become
/// `EXTINF` — but audio has to be identified to be carried along faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

/// The two codecs a plurx copy session can produce, and the only two whose
/// keyframes this module knows how to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Hevc,
    H264,
}

/// One track, as the `moov` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    /// Ticks per second for this track's durations. Video's is the one
    /// `EXTINF` is computed in — audio's differs by up to one AAC frame per
    /// fragment and using it drifts the playlist against the media.
    pub timescale: u32,
    pub codec: Option<VideoCodec>,
    /// Bytes of the length prefix on each NAL unit inside a sample, from
    /// `hvcC`/`avcC`. Zero when the track has no such record.
    pub nal_length_size: u8,
    /// `trex` defaults — the last resort when neither `trun` nor `tfhd`
    /// carries a value. ffmpeg always fills `tfhd`, so these are insurance.
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}

/// `ftyp` + `moov`: the initialization segment, kept verbatim.
///
/// Verbatim matters. This is written out as `init.mp4` unchanged, so the
/// sample entries, `hvcC`, edit lists and everything else a decoder configures
/// itself from are exactly what ffmpeg wrote — the segmenter never has an
/// opinion about them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Init {
    pub bytes: Vec<u8>,
    pub tracks: Vec<Track>,
}

impl Init {
    /// The video track, or `None` on an audio-only stream (which the copy
    /// path never produces, but a parser should not assume).
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    pub fn track(&self, id: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }
}

/// One sample, with every value resolved.
///
/// "Resolved" is the point: MP4 lets a value live in the `trun`, or in the
/// `tfhd` defaults, or in the `trex` defaults, or (for sample zero only) in
/// the `trun`'s first-sample-flags. Resolving at parse time means the merger
/// and the classifier never re-implement that ladder, and the merged output
/// can write every value explicitly and be done with defaults entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
    /// Composition offset (pts − dts). Signed here even though ffmpeg writes
    /// version-0 unsigned offsets over a shifted dts, because a version-1
    /// `trun` is legal and negative is what it means.
    pub cto: i64,
}

impl Sample {
    /// Whether this sample is a sync sample — bit 16 of `sample_flags` is
    /// `sample_is_non_sync_sample`, so the test is inverted.
    pub fn is_sync(&self) -> bool {
        self.flags & 0x0001_0000 == 0
    }
}

/// One `trun`, resolved: where its sample data starts and what its samples are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Byte offset of this run's first sample, relative to the start of the
    /// enclosing [`Fragment`]'s bytes (which begin at the `moof` — the same
    /// origin `default-base-is-moof` gives the `trun`).
    pub data_offset: usize,
    pub samples: Vec<Sample>,
}

impl Run {
    fn byte_len(&self) -> usize {
        self.samples.iter().map(|s| s.size as usize).sum()
    }
}

/// One track's share of one fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackFragment {
    pub track_id: u32,
    /// `tfdt` — the decode time of this fragment's first sample.
    pub base_decode_time: u64,
    pub runs: Vec<Run>,
}

impl TrackFragment {
    pub fn samples(&self) -> impl Iterator<Item = &Sample> {
        self.runs.iter().flat_map(|r| r.samples.iter())
    }

    pub fn sample_count(&self) -> usize {
        self.runs.iter().map(|r| r.samples.len()).sum()
    }

    /// Summed sample durations, in this track's timescale. This — not wall
    /// clock, not the audio track — is what `EXTINF` is made of.
    pub fn duration(&self) -> u64 {
        self.samples().map(|s| s.duration as u64).sum()
    }
}

/// One `moof` + `mdat` pair: with `frag_keyframe`, exactly one GOP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// The fragment verbatim, `moof` through end of `mdat`.
    pub bytes: Vec<u8>,
    /// The `mdat` payload's range within [`Fragment::bytes`] — i.e. past the
    /// box header. Every `Run::data_offset` points inside it.
    pub mdat_payload: Range<usize>,
    pub tracks: Vec<TrackFragment>,
}

impl Fragment {
    pub fn track(&self, id: u32) -> Option<&TrackFragment> {
        self.tracks.iter().find(|t| t.track_id == id)
    }

    /// Bytes on the wire, which is what the segment byte ceiling counts.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Video duration in the video track's timescale, or 0 if this fragment
    /// carries no video.
    pub fn video_duration(&self, init: &Init) -> u64 {
        init.video()
            .and_then(|v| self.track(v.id))
            .map(|t| t.duration())
            .unwrap_or(0)
    }
}

/// What [`FragmentReader::next_unit`] hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    Init(Init),
    Fragment(Fragment),
    /// The `mfra` random-access index ffmpeg appends at EOF. Recognised so it
    /// is never mistaken for payload; it is never published.
    Trailer,
}

/// Splits ffmpeg's `-f mp4 … pipe:` output into init, fragments and trailer.
///
/// Streaming and patient. `delay_moov` means the pipe opens with `ftyp` and
/// then goes quiet until the first packets commit, and a SIGSTOPped ffmpeg
/// (the ahead-window suspend) can leave minutes between reads — so this holds
/// whatever it has and answers `Ok(None)` until a whole unit has arrived,
/// forever if need be.
#[derive(Debug, Default)]
pub struct FragmentReader {
    buf: Vec<u8>,
    pos: usize,
    /// Where `ftyp` began, so `init.mp4` is `ftyp`+`moov` and not any padding
    /// that preceded them. Only meaningful before the init is emitted.
    init_start: Option<usize>,
    init_done: bool,
    saw_trailer: bool,
}

impl FragmentReader {
    pub fn new() -> FragmentReader {
        FragmentReader::default()
    }

    /// Feed bytes read from the pipe.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Bytes held but not yet formed into a unit. The session watches this:
    /// the byte ceiling bounds a segment, so a reader holding far more than
    /// one segment's worth is a bug, not a busy moment.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// The next complete unit, or `Ok(None)` when more bytes are needed.
    ///
    /// `Ok(None)` on a truncated feed is deliberate and permanent: a killed
    /// session's last partial fragment is not an error to report, it is a
    /// fragment that will never finish.
    pub fn next_unit(&mut self) -> Result<Option<Unit>, Fmp4Error> {
        loop {
            let Some(hdr) = peek_box(&self.buf, self.pos)? else {
                return Ok(None);
            };
            if !self.have(&hdr) {
                return Ok(None);
            }

            if !self.init_done {
                match hdr.kind() {
                    b"ftyp" => {
                        if self.init_start.is_none() {
                            self.init_start = Some(self.pos);
                        }
                        self.pos += hdr.size;
                    }
                    b"moov" => {
                        let start = self.init_start.unwrap_or(self.pos);
                        let end = self.pos + hdr.size;
                        let tracks = parse_moov(&self.buf[self.pos + hdr.header_len..end])?;
                        let bytes = self.buf[start..end].to_vec();
                        self.pos = end;
                        self.init_done = true;
                        self.init_start = None;
                        self.compact();
                        return Ok(Some(Unit::Init(Init { bytes, tracks })));
                    }
                    // `free`/`skip` padding before the real boxes is legal and
                    // must not become part of `init.mp4`.
                    _ => self.pos += hdr.size,
                }
                continue;
            }

            match hdr.kind() {
                b"moof" => {
                    let mdat_pos = self.pos + hdr.size;
                    let Some(mdat) = peek_box(&self.buf, mdat_pos)? else {
                        return Ok(None);
                    };
                    if mdat.kind() != b"mdat" {
                        return malformed(format!(
                            "expected mdat after moof, found {}",
                            fourcc(mdat.kind())
                        ));
                    }
                    if self.buf.len() < mdat_pos + mdat.size {
                        return Ok(None);
                    }
                    let end = mdat_pos + mdat.size;
                    let tracks =
                        parse_moof(&self.buf[self.pos..self.pos + hdr.size], hdr.header_len)?;
                    let bytes = self.buf[self.pos..end].to_vec();
                    let payload_start = hdr.size + mdat.header_len;
                    let fragment = Fragment {
                        mdat_payload: payload_start..bytes.len(),
                        bytes,
                        tracks,
                    };
                    self.pos = end;
                    self.compact();
                    return Ok(Some(Unit::Fragment(fragment)));
                }
                b"mfra" => {
                    self.pos += hdr.size;
                    self.saw_trailer = true;
                    self.compact();
                    return Ok(Some(Unit::Trailer));
                }
                // `sidx`, `free`, `skip`, a stray `styp` — none of them are
                // payload and none of them are errors.
                _ => self.pos += hdr.size,
            }
        }
    }

    /// True once the `mfra` index has been seen — i.e. ffmpeg finished
    /// cleanly rather than being killed.
    pub fn saw_trailer(&self) -> bool {
        self.saw_trailer
    }

    fn have(&self, hdr: &BoxHeader) -> bool {
        self.buf.len() >= self.pos + hdr.size
    }

    /// Drop what has been consumed. Only ever called right after a unit is
    /// emitted, so the memmove covers one partial box, never the backlog.
    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BoxHeader {
    size: usize,
    header_len: usize,
    ty: [u8; 4],
}

impl BoxHeader {
    fn kind(&self) -> &[u8; 4] {
        &self.ty
    }
}

fn fourcc(ty: &[u8; 4]) -> String {
    ty.iter()
        .map(|b| {
            if b.is_ascii_graphic() {
                *b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// Read a box header at `pos`. `Ok(None)` means "not enough bytes yet".
fn peek_box(buf: &[u8], pos: usize) -> Result<Option<BoxHeader>, Fmp4Error> {
    if buf.len() < pos + 8 {
        return Ok(None);
    }
    let size32 = be_u32(buf, pos) as usize;
    let mut ty = [0u8; 4];
    ty.copy_from_slice(&buf[pos + 4..pos + 8]);
    let (size, header_len) = match size32 {
        1 => {
            if buf.len() < pos + 16 {
                return Ok(None);
            }
            let large = be_u64(buf, pos + 8);
            if large > usize::MAX as u64 {
                return malformed("64-bit box size out of range");
            }
            (large as usize, 16)
        }
        // "to end of file" is legal in a file and meaningless in a stream that
        // has not ended. ffmpeg never writes it for the boxes on this path.
        0 => {
            return Err(Fmp4Error::Unsupported(format!(
                "box {} with size 0 (extends to EOF)",
                fourcc(&ty)
            )))
        }
        n => (n, 8),
    };
    if size < header_len {
        return malformed(format!("box {} declares size {size}", fourcc(&ty)));
    }
    Ok(Some(BoxHeader {
        size,
        header_len,
        ty,
    }))
}

/// A child box: its header and the half-open range of its payload within the
/// parent slice. Ranges are returned as a pair rather than a `Range` so
/// callers can index a slice twice without cloning.
type Child = (BoxHeader, usize, usize);

/// Iterate the child boxes of a payload slice.
fn children(payload: &[u8]) -> Result<Vec<Child>, Fmp4Error> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 8 <= payload.len() {
        let Some(hdr) = peek_box(payload, p)? else {
            break;
        };
        let Some(end) = p.checked_add(hdr.size) else {
            return malformed("box size overflows");
        };
        if end > payload.len() {
            return malformed(format!("box {} runs past its parent", fourcc(hdr.kind())));
        }
        out.push((hdr, p + hdr.header_len, end));
        p = end;
    }
    Ok(out)
}

fn be_u32(b: &[u8], p: usize) -> u32 {
    u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
}

fn be_u64(b: &[u8], p: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[p..p + 8]);
    u64::from_be_bytes(a)
}

// ---------------------------------------------------------------------------
// moov
// ---------------------------------------------------------------------------

/// Parse the track table out of a `moov` payload.
fn parse_moov(payload: &[u8]) -> Result<Vec<Track>, Fmp4Error> {
    let mut tracks = Vec::new();
    let mut trex: Vec<(u32, u32, u32, u32)> = Vec::new();
    for (hdr, start, end) in children(payload)? {
        match hdr.kind() {
            b"trak" => tracks.push(parse_trak(&payload[start..end])?),
            b"mvex" => {
                let mvex = &payload[start..end];
                for (h2, s2, e2) in children(mvex)? {
                    if h2.kind() != b"trex" {
                        continue;
                    }
                    let b = &mvex[s2..e2];
                    if b.len() < 24 {
                        return malformed("trex too short");
                    }
                    trex.push((be_u32(b, 4), be_u32(b, 12), be_u32(b, 16), be_u32(b, 20)));
                }
            }
            _ => {}
        }
    }
    if tracks.is_empty() {
        return malformed("moov declares no tracks");
    }
    for t in &mut tracks {
        if let Some((_, dur, size, flags)) = trex.iter().find(|(id, ..)| *id == t.id) {
            t.default_sample_duration = *dur;
            t.default_sample_size = *size;
            t.default_sample_flags = *flags;
        }
    }
    Ok(tracks)
}

fn parse_trak(payload: &[u8]) -> Result<Track, Fmp4Error> {
    let mut track = Track {
        id: 0,
        kind: TrackKind::Other,
        timescale: 0,
        codec: None,
        nal_length_size: 0,
        default_sample_duration: 0,
        default_sample_size: 0,
        default_sample_flags: 0,
    };
    for (hdr, start, end) in children(payload)? {
        let b = &payload[start..end];
        match hdr.kind() {
            b"tkhd" => {
                // version 0: creation(4) modification(4) track_id(4)
                // version 1: creation(8) modification(8) track_id(4)
                let off = if b.first().copied().unwrap_or(0) == 1 {
                    20
                } else {
                    12
                };
                if b.len() < off + 4 {
                    return malformed("tkhd too short");
                }
                track.id = be_u32(b, off);
            }
            b"mdia" => parse_mdia(b, &mut track)?,
            _ => {}
        }
    }
    if track.id == 0 {
        return malformed("trak has no track_id");
    }
    Ok(track)
}

fn parse_mdia(payload: &[u8], track: &mut Track) -> Result<(), Fmp4Error> {
    for (hdr, start, end) in children(payload)? {
        let b = &payload[start..end];
        match hdr.kind() {
            b"mdhd" => {
                let off = if b.first().copied().unwrap_or(0) == 1 {
                    20
                } else {
                    12
                };
                if b.len() < off + 4 {
                    return malformed("mdhd too short");
                }
                track.timescale = be_u32(b, off);
            }
            b"hdlr" => {
                if b.len() >= 12 {
                    track.kind = match &b[8..12] {
                        b"vide" => TrackKind::Video,
                        b"soun" => TrackKind::Audio,
                        _ => TrackKind::Other,
                    };
                }
            }
            b"minf" => {
                for (h2, s2, e2) in children(b)? {
                    if h2.kind() == b"stbl" {
                        parse_stbl(&b[s2..e2], track)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_stbl(payload: &[u8], track: &mut Track) -> Result<(), Fmp4Error> {
    for (hdr, start, end) in children(payload)? {
        if hdr.kind() != b"stsd" {
            continue;
        }
        // stsd payload: version/flags(4) entry_count(4) then sample entries.
        let b = &payload[start..end];
        if b.len() < 8 {
            return malformed("stsd too short");
        }
        let entries = &b[8..];
        for (entry, estart, eend) in children(entries)? {
            let codec = match entry.kind() {
                b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => Some(VideoCodec::Hevc),
                b"avc1" | b"avc3" | b"dva1" | b"dvav" => Some(VideoCodec::H264),
                _ => None,
            };
            let Some(codec) = codec else { continue };
            track.codec = Some(codec);
            // VisualSampleEntry: 8 bytes of reserved + data_reference_index,
            // then 70 bytes of fixed video fields before the child boxes. The
            // payload range already skips the 8-byte box header.
            let body = &entries[estart..eend];
            if body.len() < 78 {
                return malformed("visual sample entry too short");
            }
            let extra = &body[78..];
            for (h2, s2, e2) in children(extra)? {
                let rec = &extra[s2..e2];
                match (h2.kind(), codec) {
                    (b"hvcC", VideoCodec::Hevc) => {
                        // Byte 21 of the HEVCDecoderConfigurationRecord packs
                        // lengthSizeMinusOne into its low two bits.
                        if rec.len() < 22 {
                            return malformed("hvcC too short");
                        }
                        track.nal_length_size = (rec[21] & 3) + 1;
                    }
                    (b"avcC", VideoCodec::H264) => {
                        if rec.len() < 5 {
                            return malformed("avcC too short");
                        }
                        track.nal_length_size = (rec[4] & 3) + 1;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// moof
// ---------------------------------------------------------------------------

/// Parse a `moof`'s track fragments. Takes the whole box, header included,
/// because every `trun` data offset is relative to its first byte.
fn parse_moof(moof: &[u8], header_len: usize) -> Result<Vec<TrackFragment>, Fmp4Error> {
    let mut out = Vec::new();
    let body = &moof[header_len..];
    for (hdr, start, end) in children(body)? {
        if hdr.kind() != b"traf" {
            continue;
        }
        out.push(parse_traf(&body[start..end])?);
    }
    if out.is_empty() {
        return malformed("moof has no traf");
    }
    Ok(out)
}

fn parse_traf(payload: &[u8]) -> Result<TrackFragment, Fmp4Error> {
    let mut track_id = 0u32;
    let mut base_decode_time = 0u64;
    let mut default_duration = 0u32;
    let mut default_size = 0u32;
    let mut default_flags = 0u32;
    let mut runs: Vec<Run> = Vec::new();
    // Where a `trun` without an explicit data offset continues from. Relative
    // to the start of the enclosing `moof`, which is exactly what
    // `default-base-is-moof` makes the base.
    let mut running_offset: usize = 0;

    for (hdr, start, end) in children(payload)? {
        let b = &payload[start..end];
        match hdr.kind() {
            b"tfhd" => {
                if b.len() < 8 {
                    return malformed("tfhd too short");
                }
                let flags = be_u32(b, 0) & 0x00ff_ffff;
                track_id = be_u32(b, 4);
                if flags & 0x00_0001 != 0 {
                    // An absolute base_data_offset only appears without
                    // `default_base_moof`, which plurx always passes. Refusing
                    // is honest: the value is a file offset a pipe reader
                    // cannot resolve.
                    return Err(Fmp4Error::Unsupported(
                        "tfhd carries an explicit base_data_offset".into(),
                    ));
                }
                let mut p = 8;
                if flags & 0x00_0002 != 0 {
                    p += 4; // sample_description_index
                }
                if flags & 0x00_0008 != 0 {
                    if b.len() < p + 4 {
                        return malformed("tfhd truncated at default_sample_duration");
                    }
                    default_duration = be_u32(b, p);
                    p += 4;
                }
                if flags & 0x00_0010 != 0 {
                    if b.len() < p + 4 {
                        return malformed("tfhd truncated at default_sample_size");
                    }
                    default_size = be_u32(b, p);
                    p += 4;
                }
                if flags & 0x00_0020 != 0 {
                    if b.len() < p + 4 {
                        return malformed("tfhd truncated at default_sample_flags");
                    }
                    default_flags = be_u32(b, p);
                }
            }
            b"tfdt" => {
                if b.is_empty() {
                    return malformed("tfdt too short");
                }
                if b[0] == 1 {
                    if b.len() < 12 {
                        return malformed("tfdt v1 too short");
                    }
                    base_decode_time = be_u64(b, 4);
                } else {
                    if b.len() < 8 {
                        return malformed("tfdt v0 too short");
                    }
                    base_decode_time = be_u32(b, 4) as u64;
                }
            }
            b"trun" => {
                let run = parse_trun(
                    b,
                    default_duration,
                    default_size,
                    default_flags,
                    running_offset,
                )?;
                running_offset = run.data_offset + run.byte_len();
                runs.push(run);
            }
            _ => {}
        }
    }
    if track_id == 0 {
        return malformed("traf has no tfhd track_id");
    }
    Ok(TrackFragment {
        track_id,
        base_decode_time,
        runs,
    })
}

fn parse_trun(
    b: &[u8],
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
    continue_from: usize,
) -> Result<Run, Fmp4Error> {
    if b.len() < 8 {
        return malformed("trun too short");
    }
    let version = b[0];
    let flags = be_u32(b, 0) & 0x00ff_ffff;
    let count = be_u32(b, 4) as usize;
    let mut p = 8;
    let data_offset = if flags & 0x00_0001 != 0 {
        if b.len() < p + 4 {
            return malformed("trun truncated at data_offset");
        }
        let v = be_u32(b, p) as i32;
        p += 4;
        if v < 0 {
            return Err(Fmp4Error::Unsupported(
                "trun with a negative data_offset".into(),
            ));
        }
        v as usize
    } else {
        continue_from
    };
    let first_sample_flags = if flags & 0x00_0004 != 0 {
        if b.len() < p + 4 {
            return malformed("trun truncated at first_sample_flags");
        }
        let v = be_u32(b, p);
        p += 4;
        Some(v)
    } else {
        None
    };

    let per = 4 * usize::from(flags & 0x00_0100 != 0)
        + 4 * usize::from(flags & 0x00_0200 != 0)
        + 4 * usize::from(flags & 0x00_0400 != 0)
        + 4 * usize::from(flags & 0x00_0800 != 0);
    let need = count.saturating_mul(per);
    if b.len() < p.saturating_add(need) {
        return malformed(format!("trun declares {count} samples it does not carry"));
    }

    let mut samples = Vec::with_capacity(count);
    for i in 0..count {
        let mut duration = default_duration;
        let mut size = default_size;
        let mut sflags = if i == 0 {
            first_sample_flags.unwrap_or(default_flags)
        } else {
            default_flags
        };
        let mut cto = 0i64;
        if flags & 0x00_0100 != 0 {
            duration = be_u32(b, p);
            p += 4;
        }
        if flags & 0x00_0200 != 0 {
            size = be_u32(b, p);
            p += 4;
        }
        if flags & 0x00_0400 != 0 {
            let v = be_u32(b, p);
            p += 4;
            // First-sample-flags win over a per-sample value for sample zero:
            // that is what the flag is for, and ffmpeg only ever sets one.
            if !(i == 0 && first_sample_flags.is_some()) {
                sflags = v;
            }
        }
        if flags & 0x00_0800 != 0 {
            let raw = be_u32(b, p);
            p += 4;
            cto = if version == 0 {
                raw as i64
            } else {
                raw as i32 as i64
            };
        }
        samples.push(Sample {
            duration,
            size,
            flags: sflags,
            cto,
        });
    }
    Ok(Run {
        data_offset,
        samples,
    })
}

// ---------------------------------------------------------------------------
// classification
// ---------------------------------------------------------------------------

/// Whether a fragment's first frame is safe to start a segment with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutClass {
    /// An IDR. Nothing before it in decode order presents after it; nothing
    /// to discard.
    CleanIdr,
    /// A CRA with no leading pictures. A random-access decoder finds nothing
    /// to throw away here either.
    CleanCra,
    /// A CRA (or BLA) carrying leading pictures — a segment starting here
    /// loses exactly one frame per boundary, which is the whole bug.
    Dirty,
    /// The fragment could not be read. Treated as [`CutClass::Dirty`]
    /// everywhere a cut is considered, and counted separately so a stream this
    /// reader does not understand shows up as a number rather than as silence.
    Unparseable,
}

impl CutClass {
    /// True when a segment may begin in front of this fragment.
    pub fn is_clean(self) -> bool {
        matches!(self, CutClass::CleanIdr | CutClass::CleanCra)
    }

    pub fn label(self) -> &'static str {
        match self {
            CutClass::CleanIdr => "idr",
            CutClass::CleanCra => "cra",
            CutClass::Dirty => "dirty",
            CutClass::Unparseable => "unparseable",
        }
    }
}

/// Classify a fragment's opening keyframe.
///
/// The rule, from `docs/SEGMENTER-PLAN.md` §4.2, is conservative in one
/// direction only: anything not positively identified as clean is dirty, so a
/// misread costs a boundary that could have been cleaner and never costs a
/// frame that should have survived.
pub fn classify(frag: &Fragment, init: &Init) -> CutClass {
    let Some(video) = init.video() else {
        return CutClass::Unparseable;
    };
    let Some(tf) = frag.track(video.id) else {
        return CutClass::Unparseable;
    };
    let Some(run) = tf.runs.first() else {
        return CutClass::Unparseable;
    };
    let Some(first) = run.samples.first() else {
        return CutClass::Unparseable;
    };
    let Some(codec) = video.codec else {
        return CutClass::Unparseable;
    };
    if video.nal_length_size == 0 {
        return CutClass::Unparseable;
    }
    let start = run.data_offset;
    let end = start.saturating_add(first.size as usize);
    if end > frag.bytes.len() {
        return CutClass::Unparseable;
    }
    let Some(nal) = first_vcl_nal(&frag.bytes[start..end], video.nal_length_size, codec) else {
        return CutClass::Unparseable;
    };
    match codec {
        VideoCodec::Hevc => match nal {
            // IDR_W_RADL / IDR_N_LP — closed, by definition.
            19 | 20 => CutClass::CleanIdr,
            // CRA_NUT and the three BLA types: IRAPs that MAY carry leading
            // pictures, so the bitstream has to be asked rather than assumed.
            16..=18 | 21 => {
                if has_leading_picture(tf) {
                    CutClass::Dirty
                } else {
                    CutClass::CleanCra
                }
            }
            // A fragment opening on a non-IRAP VCL NAL should not happen under
            // `frag_keyframe`; if it does, it is emphatically not a cut point.
            _ => CutClass::Dirty,
        },
        VideoCodec::H264 => {
            if nal == 5 {
                CutClass::CleanIdr
            } else {
                // x264's open GOP signals recovery points rather than a
                // distinct NAL type, so only an IDR is provably clean here.
                CutClass::Dirty
            }
        }
    }
}

/// The first VCL NAL in a length-prefixed sample — SEI, AUD and any parameter
/// sets that survived the strip are stepped over, and the first slice decides.
fn first_vcl_nal(sample: &[u8], length_size: u8, codec: VideoCodec) -> Option<u8> {
    let lsz = length_size as usize;
    let mut q = 0usize;
    while q + lsz < sample.len() {
        let mut nlen = 0usize;
        for i in 0..lsz {
            nlen = (nlen << 8) | sample[q + i] as usize;
        }
        let head = *sample.get(q + lsz)?;
        let ty = match codec {
            VideoCodec::Hevc => (head >> 1) & 0x3f,
            VideoCodec::H264 => head & 0x1f,
        };
        let is_vcl = match codec {
            VideoCodec::Hevc => ty <= 31,
            VideoCodec::H264 => (1..=5).contains(&ty),
        };
        if is_vcl {
            return Some(ty);
        }
        if nlen == 0 {
            return None;
        }
        q = q.checked_add(lsz)?.checked_add(nlen)?;
    }
    None
}

/// Does any sample in this fragment present before its first one?
///
/// That is the whole definition of a leading picture, and it is measured in
/// presentation time rather than in composition offsets: ffmpeg shifts dts so
/// every offset is non-negative, so `pts(0)` is emphatically not the minimum
/// on a dirty fragment — which is exactly the point.
fn has_leading_picture(tf: &TrackFragment) -> bool {
    let mut dts = 0i64;
    let mut first_pts: Option<i64> = None;
    for s in tf.samples() {
        let pts = dts + s.cto;
        match first_pts {
            None => first_pts = Some(pts),
            Some(p0) if pts < p0 => return true,
            _ => {}
        }
        dts += s.duration as i64;
    }
    false
}

// ---------------------------------------------------------------------------
// cut policy
// ---------------------------------------------------------------------------

/// Why a segment ended, so the session can report honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutReason {
    /// Past the duration floor and the next fragment opens on a true
    /// random-access point. The good case, and the reason this exists.
    Clean,
    /// The accumulated bytes reached the ceiling before any clean point
    /// appeared. The cut is taken anyway — a segment that never ends is worse
    /// than a dropped frame — and counted.
    ByteCeiling,
    /// Same, for the duration ceiling that keeps `TARGETDURATION` sane on
    /// low-bitrate sources.
    TimeCeiling,
    /// End of stream: whatever is left becomes the last segment.
    EndOfStream,
}

impl CutReason {
    pub fn is_clean(self) -> bool {
        self == CutReason::Clean
    }

    pub fn label(self) -> &'static str {
        match self {
            CutReason::Clean => "clean",
            CutReason::ByteCeiling => "byte-ceiling",
            CutReason::TimeCeiling => "time-ceiling",
            CutReason::EndOfStream => "eof",
        }
    }
}

/// When to end a segment. Pure, so the whole policy is one unit test away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPolicy {
    /// Floor, in video timescale ticks, before a clean cut may be taken.
    pub floor_ticks: u64,
    /// Hard ceiling in bytes. Binding at 69 Mb/s long before any duration is.
    pub max_bytes: usize,
    /// Secondary ceiling in ticks, for sources whose bytes never pile up.
    pub max_ticks: u64,
}

impl CutPolicy {
    /// Build from the shipped constants and the stream's own video timescale.
    pub fn new(
        floor_seconds: u32,
        max_bytes: usize,
        max_seconds: u32,
        timescale: u32,
    ) -> CutPolicy {
        let ts = timescale.max(1) as u64;
        CutPolicy {
            floor_ticks: floor_seconds as u64 * ts,
            max_bytes,
            max_ticks: max_seconds as u64 * ts,
        }
    }

    /// Should the fragments accumulated so far be published *before* the
    /// fragment that just arrived?
    ///
    /// `None` means keep accumulating. Note the order: a clean cut past the
    /// floor wins, so a boundary that could be clean is never spent as a
    /// ceiling cut. And a ceiling that happens to land in front of a clean
    /// fragment is reported as clean, because the ceiling decided only *when*,
    /// not *where*.
    pub fn cut_before(
        &self,
        pending_ticks: u64,
        pending_bytes: usize,
        next: CutClass,
    ) -> Option<CutReason> {
        if pending_bytes == 0 {
            return None;
        }
        if pending_ticks >= self.floor_ticks && next.is_clean() {
            return Some(CutReason::Clean);
        }
        if pending_bytes >= self.max_bytes {
            return Some(if next.is_clean() {
                CutReason::Clean
            } else {
                CutReason::ByteCeiling
            });
        }
        if pending_ticks >= self.max_ticks {
            return Some(if next.is_clean() {
                CutReason::Clean
            } else {
                CutReason::TimeCeiling
            });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

/// What merging a run of fragments cost in fidelity. Anything non-zero here is
/// worth a log line; on a stream ffmpeg produced in one pass it is all zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// Times a source `tfdt` did not equal the previous fragment's end and the
    /// join had to be absorbed into a sample duration.
    pub tfdt_adjustments: u32,
}

/// The merged segment, and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub bytes: Vec<u8>,
    pub stats: MergeStats,
    /// Video duration in the video track's timescale — the number `EXTINF` is
    /// written from.
    pub video_ticks: u64,
}

/// One output `trun`: a source run's samples, and where its bytes came from.
struct PlanRun {
    /// Index into the merged fragment list.
    fragment: usize,
    /// The run's data offset within that fragment's own bytes.
    src_offset: usize,
    samples: Vec<Sample>,
}

struct Plan {
    track_id: u32,
    base_decode_time: u64,
    runs: Vec<PlanRun>,
}

/// Merge consecutive fragments into one `styp moof mdat` HLS segment.
///
/// The shape, and why. One `traf` per track, and inside it one `trun` per
/// source run rather than one merged `trun`: ffmpeg writes each track's
/// samples contiguously *within* a fragment but interleaves the tracks between
/// fragments, so a single `trun` — which can carry only one data offset —
/// would force the sample data to be de-interleaved and rewritten. Keeping a
/// `trun` per source run lets each `mdat` payload be copied whole and every
/// offset be arithmetic, which is what makes `framemd5` equality with the
/// unsegmented stream provable rather than hoped for.
///
/// Everything else is normalized: `tfhd` carries only `default-base-is-moof`,
/// `tfdt` is version 1, and every sample's duration, size, flags and
/// composition offset is written explicitly. So the default-value and
/// first-sample-flags ladders exist only in the reader, and nothing downstream
/// has to re-derive them.
pub fn merge(fragments: &[Fragment], init: &Init, sequence: u32) -> Result<Segment, Fmp4Error> {
    if fragments.is_empty() {
        return malformed("nothing to merge");
    }
    let mut stats = MergeStats::default();

    // Track order comes from the first fragment, which is ffmpeg's own
    // interleave order — video first. Keeping it makes a diff against hlsenc's
    // output readable.
    let track_ids: Vec<u32> = fragments[0].tracks.iter().map(|t| t.track_id).collect();
    let mut plans: Vec<Plan> = Vec::with_capacity(track_ids.len());

    for &id in &track_ids {
        let mut runs: Vec<PlanRun> = Vec::new();
        let mut base: Option<u64> = None;
        let mut expected_next: Option<u64> = None;
        for (fi, frag) in fragments.iter().enumerate() {
            let Some(tf) = frag.track(id) else { continue };
            match base {
                None => base = Some(tf.base_decode_time),
                Some(_) => {
                    // One `traf` carries one `tfdt`, so every later fragment's
                    // decode time has to be reachable by summing durations. It
                    // always is on a stream ffmpeg produced in one pass; when
                    // it is not, the join goes into the previous fragment's
                    // last sample rather than silently shifting everything
                    // after it.
                    if let Some(exp) = expected_next {
                        if tf.base_decode_time != exp {
                            let delta = tf.base_decode_time as i64 - exp as i64;
                            if let Some(last) = runs.last_mut().and_then(|r| r.samples.last_mut()) {
                                let adjusted = last.duration as i64 + delta;
                                if (0..=u32::MAX as i64).contains(&adjusted) {
                                    last.duration = adjusted as u32;
                                    stats.tfdt_adjustments += 1;
                                }
                            }
                        }
                    }
                }
            }
            let mut dur = 0u64;
            for run in &tf.runs {
                if run.samples.is_empty() {
                    continue;
                }
                dur += run.samples.iter().map(|s| s.duration as u64).sum::<u64>();
                runs.push(PlanRun {
                    fragment: fi,
                    src_offset: run.data_offset,
                    samples: run.samples.clone(),
                });
            }
            expected_next = Some(tf.base_decode_time + dur);
        }
        if let (Some(base), false) = (base, runs.is_empty()) {
            plans.push(Plan {
                track_id: id,
                base_decode_time: base,
                runs,
            });
        }
    }
    if plans.is_empty() {
        return malformed("no track survived the merge");
    }

    // Pass 1: build the moof with zeroed data offsets to learn its size. Every
    // field that could change the size is fixed-width, so pass 2 only patches
    // values — the layout is already final.
    let mut offset_slots: Vec<usize> = Vec::new();
    let mut moof = build_moof(&plans, sequence, &mut offset_slots);
    let moof_size = moof.len();

    // Where each source fragment's mdat payload lands inside the new one.
    let mut payload_base: Vec<usize> = Vec::with_capacity(fragments.len());
    let mut payload_total = 0usize;
    for frag in fragments {
        payload_base.push(payload_total);
        payload_total += frag.mdat_payload.len();
    }
    let Some(mdat_size) = payload_total.checked_add(8) else {
        return malformed("merged mdat size overflows");
    };
    if mdat_size > u32::MAX as usize {
        return Err(Fmp4Error::Unsupported(format!(
            "merged mdat would be {mdat_size} bytes"
        )));
    }

    // Pass 2: patch each trun's data offset. A run's samples start at the same
    // place in the merged payload as they did in their own fragment's, shifted
    // by where that fragment's payload landed.
    let mut slot = 0usize;
    for plan in &plans {
        for run in &plan.runs {
            let frag = &fragments[run.fragment];
            let within = run.src_offset.saturating_sub(frag.mdat_payload.start);
            let offset = moof_size + 8 + payload_base[run.fragment] + within;
            if offset > i32::MAX as usize {
                return Err(Fmp4Error::Unsupported(
                    "merged segment exceeds a 32-bit trun data offset".into(),
                ));
            }
            let pos = offset_slots[slot];
            moof[pos..pos + 4].copy_from_slice(&(offset as u32).to_be_bytes());
            slot += 1;
        }
    }

    let mut out = Vec::with_capacity(STYP.len() + moof_size + mdat_size);
    out.extend_from_slice(&STYP);
    out.extend_from_slice(&moof);
    out.extend_from_slice(&(mdat_size as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for frag in fragments {
        out.extend_from_slice(&frag.bytes[frag.mdat_payload.clone()]);
    }

    let video_ticks = init
        .video()
        .and_then(|v| plans.iter().find(|p| p.track_id == v.id))
        .map(|p| {
            p.runs
                .iter()
                .flat_map(|r| r.samples.iter())
                .map(|s| s.duration as u64)
                .sum()
        })
        .unwrap_or(0);

    Ok(Segment {
        bytes: out,
        stats,
        video_ticks,
    })
}

/// The 24-byte `styp` hlsenc writes, copied exactly: major brand `msdh`,
/// compatible with `msdh` and `msix`. Emitting it — and omitting hlsenc's two
/// `sidx` boxes, which are optional in HLS fMP4 and which hls.js's passthrough
/// ignores — is the one deliberate byte-level divergence from the muxer this
/// replaces.
const STYP: [u8; 24] = [
    0, 0, 0, 24, b's', b't', b'y', b'p', b'm', b's', b'd', b'h', 0, 0, 0, 0, b'm', b's', b'd',
    b'h', b'm', b's', b'i', b'x',
];

/// Video runs get composition offsets; audio does not. Decided per run by
/// whether any sample actually carries one, so an audio track never pays four
/// bytes a sample for a column of zeros.
fn trun_flags(samples: &[Sample]) -> u32 {
    // data-offset + duration + size + flags, all present, for every sample.
    let base = 0x0000_0701;
    if samples.iter().any(|s| s.cto != 0) {
        base | 0x0000_0800
    } else {
        base
    }
}

/// Serialize the `moof`, recording where each `trun`'s data offset field
/// landed so the caller can patch it once the total size is known.
fn build_moof(plans: &[Plan], sequence: u32, offset_slots: &mut Vec<usize>) -> Vec<u8> {
    let mut moof: Vec<u8> = Vec::new();
    moof.extend_from_slice(&[0, 0, 0, 0]); // size, patched at the end
    moof.extend_from_slice(b"moof");
    moof.extend_from_slice(&16u32.to_be_bytes());
    moof.extend_from_slice(b"mfhd");
    moof.extend_from_slice(&0u32.to_be_bytes());
    moof.extend_from_slice(&sequence.to_be_bytes());

    for plan in plans {
        let traf_start = moof.len();
        moof.extend_from_slice(&[0, 0, 0, 0]);
        moof.extend_from_slice(b"traf");
        // tfhd: default-base-is-moof and nothing else. Every default it could
        // carry is written per sample instead.
        moof.extend_from_slice(&16u32.to_be_bytes());
        moof.extend_from_slice(b"tfhd");
        moof.extend_from_slice(&0x0002_0000u32.to_be_bytes());
        moof.extend_from_slice(&plan.track_id.to_be_bytes());
        // tfdt v1 — 64-bit, because a long film in a 90 kHz timescale outgrows
        // 32 bits, and because it is what ffmpeg writes.
        moof.extend_from_slice(&20u32.to_be_bytes());
        moof.extend_from_slice(b"tfdt");
        moof.extend_from_slice(&0x0100_0000u32.to_be_bytes());
        moof.extend_from_slice(&plan.base_decode_time.to_be_bytes());

        for run in &plan.runs {
            let flags = trun_flags(&run.samples);
            let has_cto = flags & 0x0000_0800 != 0;
            let per = if has_cto { 16 } else { 12 };
            let size = 8 + 4 + 4 + 4 + run.samples.len() * per;
            moof.extend_from_slice(&(size as u32).to_be_bytes());
            moof.extend_from_slice(b"trun");
            moof.extend_from_slice(&flags.to_be_bytes());
            moof.extend_from_slice(&(run.samples.len() as u32).to_be_bytes());
            offset_slots.push(moof.len());
            moof.extend_from_slice(&0u32.to_be_bytes()); // data_offset, patched
            for s in &run.samples {
                moof.extend_from_slice(&s.duration.to_be_bytes());
                moof.extend_from_slice(&s.size.to_be_bytes());
                moof.extend_from_slice(&s.flags.to_be_bytes());
                if has_cto {
                    // Version 0, so the offset is unsigned: ffmpeg shifts dts
                    // to keep every one non-negative, and a stream where that
                    // was not true would need a v1 trun.
                    moof.extend_from_slice(&(s.cto.max(0) as u32).to_be_bytes());
                }
            }
        }
        let traf_len = (moof.len() - traf_start) as u32;
        moof[traf_start..traf_start + 4].copy_from_slice(&traf_len.to_be_bytes());
    }
    let total = moof.len() as u32;
    moof[0..4].copy_from_slice(&total.to_be_bytes());
    moof
}

// ---------------------------------------------------------------------------
// the segmenter
// ---------------------------------------------------------------------------

/// What a session's segmenter has done so far. This is the end-of-session log
/// line, and `scripts/perf-report` greps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SegmentCounts {
    pub fragments: u64,
    pub segments: u64,
    /// Cuts taken in front of a true random-access point — the ones that cost
    /// no frame.
    pub clean_cuts: u64,
    /// Cuts forced by the byte or duration ceiling with no clean point in
    /// reach. Each one still costs the leading picture, and saying so is the
    /// difference between a measured residual and a silent one.
    pub ceiling_cuts: u64,
    /// Fragments this reader could not classify. Never cut in front of;
    /// counted because a stream shape nobody anticipated should show up as a
    /// number rather than as silence.
    pub unparseable: u64,
    pub tfdt_adjustments: u32,
}

/// One segment, ready to write.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub index: u64,
    pub segment: Segment,
    pub reason: CutReason,
    /// `EXTINF`, from summed video sample durations in the video timescale.
    pub seconds: f64,
}

impl Published {
    pub fn name(&self) -> String {
        segment_name(self.index)
    }
}

/// Accumulates fragments and publishes segments that start on clean keyframes.
///
/// Pure by design: the daemon feeds it fragments and writes what comes back
/// out, so every rule about where a boundary may land is testable without a
/// filesystem, a process or a clock.
#[derive(Debug)]
pub struct Segmenter {
    init: Init,
    policy: CutPolicy,
    video_timescale: u32,
    pending: Vec<Fragment>,
    pending_bytes: usize,
    pending_ticks: u64,
    next_index: u64,
    counts: SegmentCounts,
}

impl Segmenter {
    pub fn new(init: Init, policy: CutPolicy) -> Segmenter {
        let video_timescale = init.video().map(|v| v.timescale).unwrap_or(0).max(1);
        Segmenter {
            init,
            policy,
            video_timescale,
            pending: Vec::new(),
            pending_bytes: 0,
            pending_ticks: 0,
            next_index: 0,
            counts: SegmentCounts::default(),
        }
    }

    pub fn init(&self) -> &Init {
        &self.init
    }

    pub fn counts(&self) -> SegmentCounts {
        self.counts
    }

    /// Bytes held back waiting for a cut point. Bounded by the byte ceiling
    /// plus one fragment; anything beyond that is worth a log line.
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Feed the next fragment. Returns a segment when this fragment's arrival
    /// is what ended the previous one — the cut is decided *in front of* the
    /// new fragment, which is the whole trick: a boundary can only be judged
    /// once you can see what comes after it.
    pub fn push(&mut self, fragment: Fragment) -> Result<Option<Published>, Fmp4Error> {
        let class = classify(&fragment, &self.init);
        self.counts.fragments += 1;
        if class == CutClass::Unparseable {
            self.counts.unparseable += 1;
        }
        let published = match self
            .policy
            .cut_before(self.pending_ticks, self.pending_bytes, class)
        {
            Some(reason) => Some(self.flush(reason)?),
            None => None,
        };
        self.pending_ticks += fragment.video_duration(&self.init);
        self.pending_bytes += fragment.len();
        self.pending.push(fragment);
        Ok(published)
    }

    /// End of stream: whatever is still pending becomes the last segment.
    pub fn finish(&mut self) -> Result<Option<Published>, Fmp4Error> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        self.flush(CutReason::EndOfStream).map(Some)
    }

    fn flush(&mut self, reason: CutReason) -> Result<Published, Fmp4Error> {
        let index = self.next_index;
        let segment = merge(&self.pending, &self.init, index as u32 + 1)?;
        self.pending.clear();
        self.pending_bytes = 0;
        self.pending_ticks = 0;
        self.next_index += 1;
        self.counts.segments += 1;
        self.counts.tfdt_adjustments += segment.stats.tfdt_adjustments;
        match reason {
            CutReason::Clean => self.counts.clean_cuts += 1,
            CutReason::ByteCeiling | CutReason::TimeCeiling => self.counts.ceiling_cuts += 1,
            // The last segment of a session did not choose where it ended, so
            // it is neither a clean cut nor a ceiling cut. Counting it as
            // either would flatter or slander the policy.
            CutReason::EndOfStream => {}
        }
        let seconds = segment.video_ticks as f64 / self.video_timescale as f64;
        Ok(Published {
            index,
            segment,
            reason,
            seconds,
        })
    }
}

// ---------------------------------------------------------------------------
// playlist
// ---------------------------------------------------------------------------

/// The playlist header, written once when the session's first segment lands.
///
/// `TARGETDURATION` is the duration ceiling from the start rather than a
/// number that grows with the longest segment so far: the tag may never
/// decrease mid-session, and a player that read a small one and then met a
/// longer segment is entitled to complain.
///
/// No `#EXT-X-INDEPENDENT-SEGMENTS`. With clean cuts the claim would actually
/// be true for most segments — but a ceiling cut makes it a lie again, and a
/// lie in a spec tag is what this whole path is here to stop shipping.
pub fn playlist_header(target_duration: u32) -> String {
    format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target_duration}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:EVENT\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n"
    )
}

/// One published segment's two lines. Six decimals, from summed video sample
/// durations in the video timescale — never wall clock, never audio.
pub fn playlist_entry(seconds: f64, name: &str) -> String {
    let mut s = String::new();
    let _ = write!(s, "#EXTINF:{seconds:.6},\n{name}\n");
    s
}

/// `seg00000.m4s` — five digits from zero, exactly what `-start_number 0` and
/// `seg%05d.m4s` gave the muxer this replaces. The daemon's segment index
/// parses these names.
pub fn segment_name(index: u64) -> String {
    format!("seg{index:05}.m4s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{LazyLock, Mutex};

    // -----------------------------------------------------------------------
    // fixtures
    // -----------------------------------------------------------------------

    /// Generated once into `target/fixtures/` and reused. They are small (12 s
    /// of 640x360) because every one of them is an x265 encode CI pays for,
    /// and none of the properties under test — GOP structure, box layout,
    /// sample timing — care how big the picture is.
    fn fixture_dir() -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // crates/
        p.pop(); // repo root
        p.push("target");
        p.push("fixtures");
        p
    }

    fn ffmpeg() -> String {
        std::env::var("PLURX_FFMPEG")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "ffmpeg".to_owned())
    }

    fn ffprobe() -> String {
        std::env::var("PLURX_FFPROBE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "ffprobe".to_owned())
    }

    /// Fail in terms of the dependency that is missing rather than in terms of
    /// whatever the parser made of an empty file. plurxd shells out to ffmpeg
    /// at runtime, so this is a thing to install, not a test to skip.
    fn require_ffmpeg() {
        let bin = ffmpeg();
        let ok = Command::new(&bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        assert!(
            ok,
            "these tests need ffmpeg; running `{bin}` failed. Install it \
             (`apt-get install ffmpeg`) or point PLURX_FFMPEG at a build — \
             plurxd requires it at runtime too"
        );
    }

    fn run(cmd: &mut Command) -> Vec<u8> {
        let out = cmd.output().expect("running ffmpeg");
        assert!(
            out.status.success(),
            "{:?} failed: {}",
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// x265 parameters per fixture. The GOP shape is the entire point of each
    /// one, so it is spelled out rather than left to the encoder's defaults.
    fn x265_params(kind: &str) -> &'static str {
        match kind {
            // Every keyframe a CRA with a RASL leading picture: the disc-remux
            // shape, and the source of the bug.
            "open-gop" => {
                "keyint=42:min-keyint=42:open-gop=1:bframes=4:b-pyramid=2:\
                           scenecut=0:repeat-headers=1:log-level=none"
            }
            // Every keyframe an IDR: the control.
            "closed-gop" => {
                "keyint=42:min-keyint=42:open-gop=0:bframes=4:scenecut=0:\
                             repeat-headers=1:log-level=none"
            }
            // Open GOP with no B-frames, so every CRA is clean — the case the
            // classifier must not confuse with the open-gop fixture.
            "clean-cra" => {
                "keyint=42:min-keyint=42:open-gop=1:bframes=0:scenecut=0:\
                            repeat-headers=1:log-level=none"
            }
            other => panic!("no x265 params for fixture {other}"),
        }
    }

    static FIXTURE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Path to a source fixture, generating it on first use.
    fn source(kind: &str) -> PathBuf {
        require_ffmpeg();
        let dir = fixture_dir();
        let path = dir.join(format!("{kind}.mkv"));
        let _guard = FIXTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if path.exists() {
            return path;
        }
        std::fs::create_dir_all(&dir).expect("creating target/fixtures");
        let tmp = dir.join(format!("{kind}.mkv.tmp"));
        let mut cmd = Command::new(ffmpeg());
        cmd.args(["-y", "-v", "error"])
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=24:duration=12",
            ])
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=12",
            ]);
        if kind == "h264" {
            cmd.args(["-c:v", "libx264", "-preset", "ultrafast"]).args([
                "-x264-params",
                "keyint=42:min-keyint=42:open_gop=1:bframes=3",
            ]);
        } else {
            cmd.args(["-c:v", "libx265", "-preset", "ultrafast"])
                .args(["-x265-params", x265_params(kind)]);
        }
        // `-f matroska` because the temp name has no extension ffmpeg knows;
        // the file is renamed into place only once it is complete, so a killed
        // run never leaves a half-written fixture for the next one to read.
        cmd.args([
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
        ])
        .args(["-f", "matroska"])
        .arg(&tmp);
        run(&mut cmd);
        std::fs::rename(&tmp, &path).expect("publishing the fixture");
        path
    }

    /// The production pipe command, exactly as `copy_pipe_args` builds it, run
    /// against a fixture. Cached beside the source: it is deterministic, and
    /// every test in this module wants the same bytes.
    fn pipe(kind: &str) -> Vec<u8> {
        let src = source(kind);
        let out = fixture_dir().join(format!("{kind}.pipe.mp4"));
        let _guard = FIXTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(bytes) = std::fs::read(&out) {
            return bytes;
        }
        let hevc = kind != "h264";
        let mut cmd = Command::new(ffmpeg());
        cmd.args(["-hide_banner", "-loglevel", "error"])
            .arg("-i")
            .arg(&src)
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-c:v", "copy"]);
        if hevc {
            cmd.args(["-tag:v", "hvc1"])
                .args(["-bsf:v", "filter_units=remove_types=32-34"]);
        }
        cmd.args(["-c:a", "aac", "-b:a", "256k"])
            .args(["-avoid_negative_ts", "make_zero"])
            .args([
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof+delay_moov",
            ])
            .args(["-f", "mp4", "pipe:1"]);
        let bytes = run(&mut cmd);
        std::fs::write(&out, &bytes).expect("caching the pipe output");
        bytes
    }

    /// Everything a feed produces, in order.
    fn read_all(feed: &[u8]) -> (Init, Vec<Fragment>, bool) {
        let mut reader = FragmentReader::new();
        reader.push(feed);
        let mut init = None;
        let mut frags = Vec::new();
        let mut trailer = false;
        while let Some(unit) = reader.next_unit().expect("parsing the pipe output") {
            match unit {
                Unit::Init(i) => init = Some(i),
                Unit::Fragment(f) => frags.push(f),
                Unit::Trailer => trailer = true,
            }
        }
        (
            init.expect("the pipe carried an init segment"),
            frags,
            trailer,
        )
    }

    /// Count top-level boxes of a type, so the reader is checked against an
    /// independent walk of the same bytes rather than against itself.
    fn count_boxes(feed: &[u8], want: &[u8; 4]) -> usize {
        let mut n = 0;
        let mut p = 0usize;
        while let Ok(Some(hdr)) = peek_box(feed, p) {
            if hdr.kind() == want {
                n += 1;
            }
            p += hdr.size;
            if p >= feed.len() {
                break;
            }
        }
        n
    }

    // -----------------------------------------------------------------------
    // M0 — the reader
    // -----------------------------------------------------------------------

    #[test]
    fn parses_the_pipes_init_and_every_fragment() {
        let feed = pipe("open-gop");
        let (init, frags, trailer) = read_all(&feed);

        // The init is the first two boxes, byte for byte — that is what makes
        // writing it out as `init.mp4` a copy rather than a re-mux.
        assert_eq!(
            init.bytes,
            &feed[..init.bytes.len()],
            "init.mp4 is not a verbatim prefix of the pipe"
        );
        let head = peek_box(&init.bytes, 0).expect("ftyp").expect("ftyp");
        assert_eq!(head.kind(), b"ftyp");
        let moov = peek_box(&init.bytes, head.size)
            .expect("moov")
            .expect("moov");
        assert_eq!(moov.kind(), b"moov");
        assert_eq!(head.size + moov.size, init.bytes.len());

        assert_eq!(init.tracks.len(), 2, "video + audio");
        let video = init.video().expect("a video track");
        assert_eq!(video.kind, TrackKind::Video);
        assert_eq!(video.codec, Some(VideoCodec::Hevc));
        assert_eq!(video.nal_length_size, 4, "hvcC lengthSizeMinusOne");
        assert!(video.timescale > 0);
        assert!(init
            .tracks
            .iter()
            .any(|t| t.kind == TrackKind::Audio && t.timescale == 48_000));

        assert_eq!(
            frags.len(),
            count_boxes(&feed, b"moof"),
            "a fragment per moof, no more and no fewer"
        );
        assert!(frags.len() >= 6, "12 s at a 1.75 s GOP: {}", frags.len());
        assert!(trailer, "ffmpeg's mfra index was not recognised");

        for (i, f) in frags.iter().enumerate() {
            assert_eq!(f.tracks.len(), 2, "fragment {i} lost a track");
            // Every sample's bytes have to lie inside the fragment, or the
            // merger's offset arithmetic is building on sand.
            for t in &f.tracks {
                for r in &t.runs {
                    assert!(
                        r.data_offset >= f.mdat_payload.start
                            && r.data_offset + r.byte_len() <= f.bytes.len(),
                        "fragment {i} track {} run runs outside its mdat",
                        t.track_id
                    );
                }
            }
        }
    }

    /// The reader resolves `tfhd` defaults and first-sample-flags into
    /// per-sample values. ffprobe reads the same file through an unrelated
    /// implementation, so agreeing with it is evidence; agreeing with our own
    /// re-parse would not be.
    ///
    /// Compared on pts, dts and size, and deliberately NOT on ffprobe's
    /// `packet=duration`: that field is derived from the stream's frame rate,
    /// not from the sample table, so on 24 fps at timescale 16000 it reads a
    /// flat 666 against real sample durations that cycle 672/672/656 to sum
    /// exactly. Per-packet dts is read from the container, and every dts here
    /// is *generated* by summing our resolved durations — so dts agreement is
    /// the duration check, arrived at from the other end. The stream's total
    /// `duration_ts` closes the one gap that leaves: the last sample.
    #[test]
    fn resolved_samples_reproduce_ffprobe_timing() {
        let feed = pipe("open-gop");
        let path = fixture_dir().join("open-gop.pipe.mp4");
        let (init, frags, _) = read_all(&feed);
        let video = init.video().expect("video").clone();

        let csv = run(Command::new(ffprobe())
            .args(["-v", "error", "-select_streams", "v:0", "-show_packets"])
            .args(["-show_entries", "packet=pts,dts,size"])
            .args(["-of", "csv=p=0"])
            .arg(&path));
        let csv = String::from_utf8_lossy(&csv);
        let expected: Vec<(i64, i64, i64)> = csv
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let f: Vec<i64> = l
                    .split(',')
                    .map(|v| v.trim().parse::<i64>().unwrap_or(i64::MIN))
                    .collect();
                (f[0], f[1], f[2])
            })
            .collect();

        let mut got: Vec<(i64, i64, i64)> = Vec::new();
        let mut total_ticks = 0u64;
        for f in &frags {
            let tf = f.track(video.id).expect("video in every fragment");
            let mut dts = tf.base_decode_time as i64;
            for s in tf.samples() {
                got.push((dts + s.cto, dts, s.size as i64));
                dts += s.duration as i64;
                total_ticks += s.duration as u64;
            }
        }

        assert_eq!(
            got.len(),
            expected.len(),
            "sample count disagrees with ffprobe"
        );
        assert_eq!(
            got, expected,
            "resolved sample timing disagrees with ffprobe"
        );

        let duration_ts = run(Command::new(ffprobe())
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", "stream=duration_ts", "-of", "csv=p=0"])
            .arg(&path));
        let duration_ts: u64 = String::from_utf8_lossy(&duration_ts)
            .trim()
            .parse()
            .expect("ffprobe duration_ts");
        assert_eq!(
            total_ticks, duration_ts,
            "summed sample durations disagree with the stream duration"
        );
    }

    #[test]
    fn a_truncated_feed_yields_no_fragment_and_no_panic() {
        let feed = pipe("open-gop");

        // Nothing but a box header.
        let mut reader = FragmentReader::new();
        reader.push(&feed[..6]);
        assert_eq!(reader.next_unit().expect("no error"), None);

        // ftyp complete, moov half-arrived: still no init.
        let mut reader = FragmentReader::new();
        reader.push(&feed[..100]);
        assert_eq!(reader.next_unit().expect("no error"), None);

        // A fragment cut in half — the init lands, nothing else does, and
        // asking again forever is still not an error. This is what a killed
        // session looks like from in here.
        let (init, frags, _) = read_all(&feed);
        let cut = init.bytes.len() + frags[0].bytes.len() + frags[1].bytes.len() / 2;
        let mut reader = FragmentReader::new();
        reader.push(&feed[..cut]);
        assert!(matches!(reader.next_unit(), Ok(Some(Unit::Init(_)))));
        assert!(matches!(reader.next_unit(), Ok(Some(Unit::Fragment(_)))));
        for _ in 0..3 {
            assert_eq!(reader.next_unit().expect("no error"), None);
        }
        assert!(!reader.saw_trailer());
    }

    /// A pipe delivers whatever the kernel felt like. Odd-sized pushes must
    /// produce identical units to one big one — a reader that only works on
    /// whole-box reads works right up until production.
    #[test]
    fn a_feed_arriving_in_odd_chunks_parses_identically() {
        let feed = pipe("open-gop");
        let (init, frags, _) = read_all(&feed);

        let mut reader = FragmentReader::new();
        let mut got_init = None;
        let mut got = Vec::new();
        for chunk in feed.chunks(1_237) {
            reader.push(chunk);
            while let Some(unit) = reader.next_unit().expect("parsing") {
                match unit {
                    Unit::Init(i) => got_init = Some(i),
                    Unit::Fragment(f) => got.push(f),
                    Unit::Trailer => {}
                }
            }
        }
        assert_eq!(got_init.as_ref(), Some(&init));
        assert_eq!(got, frags);
        assert_eq!(reader.buffered(), 0, "bytes left over after a whole feed");
    }

    // -----------------------------------------------------------------------
    // M1 — the classifier
    // -----------------------------------------------------------------------

    fn classes(kind: &str) -> Vec<CutClass> {
        let feed = pipe(kind);
        let (init, frags, _) = read_all(&feed);
        frags.iter().map(|f| classify(f, &init)).collect()
    }

    /// The disc-remux shape: the stream opens on an IDR and every keyframe
    /// after it is a CRA carrying a RASL. This is the source of the bug, and
    /// the classifier has to see it for what it is.
    #[test]
    fn every_cra_on_the_open_gop_fixture_is_dirty() {
        let c = classes("open-gop");
        assert_eq!(c[0], CutClass::CleanIdr, "the stream must open on an IDR");
        assert!(
            c[1..].iter().all(|k| *k == CutClass::Dirty),
            "an open-GOP CRA with a leading picture was called clean: {c:?}"
        );
        assert!(!c.contains(&CutClass::Unparseable));
    }

    #[test]
    fn every_keyframe_on_the_closed_gop_fixture_is_clean() {
        let c = classes("closed-gop");
        assert!(
            c.iter().all(|k| *k == CutClass::CleanIdr),
            "a closed-GOP keyframe was not read as an IDR: {c:?}"
        );
    }

    /// A CRA is not dirty by virtue of being a CRA — only by carrying leading
    /// pictures. With no B-frames there are none, and calling these dirty
    /// would spend ceiling cuts on a source that needs none.
    #[test]
    fn a_cra_with_no_leading_pictures_is_clean() {
        let c = classes("clean-cra");
        assert_eq!(c[0], CutClass::CleanIdr);
        assert!(
            c[1..].iter().all(|k| *k == CutClass::CleanCra),
            "a leading-picture-free CRA was not called clean: {c:?}"
        );
    }

    /// x264's open GOP signals recovery points rather than a distinct NAL
    /// type, so there is nothing in the bitstream to prove a non-IDR keyframe
    /// safe. Only NAL 5 is clean, and the rest are dirty by policy.
    #[test]
    fn h264_only_idr_is_clean() {
        let c = classes("h264");
        assert_eq!(c[0], CutClass::CleanIdr);
        assert!(
            c[1..].iter().all(|k| *k == CutClass::Dirty),
            "an h264 non-IDR keyframe was called clean: {c:?}"
        );
    }

    #[test]
    fn the_policy_waits_past_the_floor_for_a_clean_fragment() {
        let p = CutPolicy::new(6, 48_000_000, 15, 1_000);
        // Under the floor, clean or not: keep going.
        assert_eq!(p.cut_before(5_000, 1_000, CutClass::CleanIdr), None);
        // Past the floor but the next fragment is dirty: still keep going,
        // which is the entire behaviour change this module ships.
        assert_eq!(p.cut_before(7_000, 1_000, CutClass::Dirty), None);
        assert_eq!(p.cut_before(7_000, 1_000, CutClass::Unparseable), None);
        // Past the floor with a clean fragment in front: cut.
        assert_eq!(
            p.cut_before(7_000, 1_000, CutClass::CleanCra),
            Some(CutReason::Clean)
        );
        // Nothing pending is not a cut, however clean the next fragment is.
        assert_eq!(p.cut_before(0, 0, CutClass::CleanIdr), None);
    }

    #[test]
    fn the_ceilings_cut_when_no_clean_point_arrives() {
        let p = CutPolicy::new(6, 48_000_000, 15, 1_000);
        assert_eq!(
            p.cut_before(7_000, 48_000_000, CutClass::Dirty),
            Some(CutReason::ByteCeiling)
        );
        assert_eq!(
            p.cut_before(15_000, 1_000, CutClass::Dirty),
            Some(CutReason::TimeCeiling)
        );
        // A ceiling that happens to land in front of a clean fragment is a
        // clean cut: the ceiling decided when, not where.
        assert_eq!(
            p.cut_before(15_000, 48_000_000, CutClass::CleanIdr),
            Some(CutReason::Clean)
        );
    }

    // -----------------------------------------------------------------------
    // M2 — the merger
    // -----------------------------------------------------------------------

    /// Run a whole feed through the segmenter, returning what a session would
    /// have published.
    fn segment(
        kind: &str,
        floor_s: u32,
        max_bytes: usize,
        max_s: u32,
    ) -> (Init, Vec<Published>, SegmentCounts) {
        let feed = pipe(kind);
        let (init, frags, _) = read_all(&feed);
        let timescale = init.video().map(|v| v.timescale).unwrap_or(1);
        let policy = CutPolicy::new(floor_s, max_bytes, max_s, timescale);
        let mut seg = Segmenter::new(init.clone(), policy);
        let mut out = Vec::new();
        for f in frags {
            if let Some(p) = seg.push(f).expect("merging") {
                out.push(p);
            }
        }
        if let Some(p) = seg.finish().expect("final merge") {
            out.push(p);
        }
        (init, out, seg.counts())
    }

    fn write_candidate(dir: &Path, init: &Init, published: &[Published]) -> PathBuf {
        let path = dir.join("candidate.mp4");
        let mut bytes = init.bytes.clone();
        for p in published {
            bytes.extend_from_slice(&p.segment.bytes);
        }
        std::fs::write(&path, &bytes).expect("writing the candidate");
        path
    }

    fn framemd5(path: &Path, stream: &str) -> String {
        let out = run(Command::new(ffmpeg())
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", stream, "-f", "framemd5", "-"]));
        String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The licence to ship: what comes out of the merger decodes to exactly
    /// the same frames, at exactly the same times, as the continuous stream it
    /// was cut from. `framemd5` covers both — it prints a hash per frame *with*
    /// its pts and duration — so a re-ordered, re-timed or dropped frame all
    /// fail it. Nothing else in this module is allowed to be interesting if
    /// this is not true.
    #[test]
    fn merged_stream_decodes_bit_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (init, published, counts) = segment("open-gop", 3, 300_000, 15);
        assert!(
            published.len() >= 2,
            "the fixture produced one segment only"
        );
        assert_eq!(counts.tfdt_adjustments, 0, "a join had to be repaired");

        let candidate = write_candidate(dir.path(), &init, &published);
        let reference = fixture_dir().join("open-gop.pipe.mp4");
        for stream in ["0:v:0", "0:a:0"] {
            assert_eq!(
                framemd5(&candidate, stream),
                framemd5(&reference, stream),
                "{stream} decodes differently after merging"
            );
        }
    }

    /// Re-parse our own output and check it against the fragments it was made
    /// from. framemd5 proves the decoder agrees; this proves the container
    /// says what we meant it to say, which is what a *different* player will
    /// read.
    #[test]
    fn every_sample_keeps_its_pts_dur_size() {
        let (init, published, _) = segment("open-gop", 3, 300_000, 15);
        let feed = pipe("open-gop");
        let (_, source_frags, _) = read_all(&feed);

        // Flatten the source into (dts, pts, duration, size, flags) per track.
        let mut want: Vec<(u32, i64, i64, u32, u32, u32)> = Vec::new();
        for f in &source_frags {
            for tf in &f.tracks {
                let mut dts = tf.base_decode_time as i64;
                for s in tf.samples() {
                    want.push((tf.track_id, dts, dts + s.cto, s.duration, s.size, s.flags));
                    dts += s.duration as i64;
                }
            }
        }

        let mut got: Vec<(u32, i64, i64, u32, u32, u32)> = Vec::new();
        for p in &published {
            let mut feed = init.bytes.clone();
            feed.extend_from_slice(&p.segment.bytes);
            let (_, frags, _) = read_all(&feed);
            assert_eq!(frags.len(), 1, "a published segment is one fragment");
            for tf in &frags[0].tracks {
                let mut dts = tf.base_decode_time as i64;
                for s in tf.samples() {
                    got.push((tf.track_id, dts, dts + s.cto, s.duration, s.size, s.flags));
                    dts += s.duration as i64;
                }
            }
            // And the bytes each sample points at are the bytes it had.
            for tf in &frags[0].tracks {
                for r in &tf.runs {
                    assert!(
                        r.data_offset + r.byte_len() <= p.segment.bytes.len(),
                        "a run points past the end of its own segment"
                    );
                }
            }
        }

        want.sort_by_key(|s| (s.0, s.1));
        got.sort_by_key(|s| (s.0, s.1));
        assert_eq!(got.len(), want.len(), "the merger lost or invented samples");
        assert_eq!(got, want, "a sample changed on the way through the merger");
    }

    /// The defect this whole investigation chased first (STUTTER-4K §3.8/§3.9)
    /// was a join overlap of one source timestamp tick. Consecutive published
    /// segments must join at exactly zero, on both tracks, in the tracks' own
    /// integer units — a float comparison would hide precisely the size of
    /// error that matters.
    #[test]
    fn joins_are_gapless_and_overlap_free() {
        let (init, published, _) = segment("open-gop", 3, 300_000, 15);
        assert!(published.len() >= 3, "not enough joins to be evidence");

        let mut end: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        for (i, p) in published.iter().enumerate() {
            let mut feed = init.bytes.clone();
            feed.extend_from_slice(&p.segment.bytes);
            let (_, frags, _) = read_all(&feed);
            for tf in &frags[0].tracks {
                if let Some(prev) = end.get(&tf.track_id) {
                    assert_eq!(
                        tf.base_decode_time,
                        *prev,
                        "segment {i} track {} starts {} ticks from where {} ended",
                        tf.track_id,
                        tf.base_decode_time as i64 - *prev as i64,
                        i - 1
                    );
                }
                end.insert(tf.track_id, tf.base_decode_time + tf.duration());
            }
        }

        // And the video timeline the playlist claims matches the media.
        let video = init.video().expect("video");
        let claimed: f64 = published.iter().map(|p| p.seconds).sum();
        let actual = end.get(&video.id).copied().unwrap_or(0) as f64 / video.timescale as f64;
        assert!(
            (claimed - actual).abs() < 1e-6,
            "EXTINF sums to {claimed}s against {actual}s of media"
        );
    }

    /// Two tracks go in, two tracks come out, with the same durations —
    /// checked through ffprobe rather than through our own parser, because the
    /// failure this guards against (an offset rebased against the wrong
    /// track's payload) produces a file our parser would still describe
    /// happily.
    #[test]
    fn audio_and_video_survive_interleaved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (init, published, _) = segment("open-gop", 3, 300_000, 15);
        let candidate = write_candidate(dir.path(), &init, &published);
        let reference = fixture_dir().join("open-gop.pipe.mp4");

        let describe = |p: &Path| {
            let out = run(Command::new(ffprobe())
                .args(["-v", "error", "-show_entries"])
                .args(["stream=index,codec_name,nb_read_packets", "-count_packets"])
                .args(["-of", "csv=p=0"])
                .arg(p));
            String::from_utf8_lossy(&out).trim().to_owned()
        };
        assert_eq!(
            describe(&candidate),
            describe(&reference),
            "the merged file does not carry the same streams as the source"
        );
    }

    /// A source with no clean point anywhere still has to make progress, and
    /// has to say that it did so the honest way.
    #[test]
    fn a_source_with_no_clean_points_cuts_at_the_ceiling_and_counts_it() {
        // Ceiling small enough that a 12 s fixture reaches it: the policy is a
        // parameter precisely so this is testable without a 4K file.
        let (_, published, counts) = segment("open-gop", 3, 300_000, 15);
        assert!(published.len() >= 2);
        assert!(
            counts.ceiling_cuts >= 1,
            "no ceiling cut on a source with no clean point: {counts:?}"
        );
        assert_eq!(counts.clean_cuts, 0, "there was no clean point to find");
        assert_eq!(counts.unparseable, 0);
        assert!(published.iter().any(|p| p.reason == CutReason::ByteCeiling));
        assert_eq!(
            published.last().map(|p| p.reason),
            Some(CutReason::EndOfStream)
        );
    }

    /// The other half: given clean points, every cut takes one.
    #[test]
    fn a_source_with_clean_points_never_cuts_dirty() {
        let (_, published, counts) = segment("clean-cra", 3, 48_000_000, 15);
        assert!(published.len() >= 2);
        assert_eq!(
            counts.ceiling_cuts, 0,
            "a ceiling cut with clean points available"
        );
        assert_eq!(counts.clean_cuts as usize, published.len() - 1);
        for p in &published[..published.len() - 1] {
            assert_eq!(p.reason, CutReason::Clean);
            // Past the floor, and not wildly past it: the cut is taken at the
            // *first* clean point after the floor, not the last.
            assert!(p.seconds >= 3.0, "cut below the floor at {}s", p.seconds);
            assert!(p.seconds < 3.0 + 2.0, "cut {}s past the floor", p.seconds);
        }
    }

    #[test]
    fn the_playlist_matches_the_template() {
        let header = playlist_header(15);
        assert!(header.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"));
        assert!(header.contains("#EXT-X-TARGETDURATION:15\n"));
        assert!(header.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n"));
        assert!(header.contains("#EXT-X-MAP:URI=\"init.mp4\"\n"));
        assert!(
            !header.contains("INDEPENDENT-SEGMENTS"),
            "a claim a ceiling cut makes false"
        );
        assert_eq!(
            playlist_entry(6.208_333, "seg00000.m4s"),
            "#EXTINF:6.208333,\nseg00000.m4s\n"
        );
        assert_eq!(segment_name(0), "seg00000.m4s");
        assert_eq!(segment_name(12_345), "seg12345.m4s");
    }

    /// The one deliberate byte-level divergence from hlsenc is the two `sidx`
    /// boxes; everything else about a segment's shape is the same. Assert the
    /// shape here so a future change to the merger has to mean it.
    #[test]
    fn a_published_segment_is_styp_moof_mdat() {
        let (_, published, _) = segment("open-gop", 3, 300_000, 15);
        let bytes = &published[0].segment.bytes;
        let mut types = Vec::new();
        let mut p = 0usize;
        while let Ok(Some(hdr)) = peek_box(bytes, p) {
            types.push(fourcc(hdr.kind()));
            p += hdr.size;
            if p >= bytes.len() {
                break;
            }
        }
        assert_eq!(types, vec!["styp", "moof", "mdat"]);
        assert_eq!(&bytes[..24], &STYP, "styp is not hlsenc's 24 bytes");
    }

    #[test]
    fn merging_nothing_is_an_error_not_an_empty_segment() {
        let feed = pipe("open-gop");
        let (init, _, _) = read_all(&feed);
        assert!(merge(&[], &init, 1).is_err());
    }
}
