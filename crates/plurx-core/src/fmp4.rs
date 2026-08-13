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
//!   normally becomes `init.mp4` verbatim), a [`Fragment`] per GOP, and the trailing
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

/// Largest box this reader will believe in.
///
/// Two jobs. It is a sanity bound — one fragment is one GOP, and a GOP that
/// serialized to two gigabytes is a stream nobody produced — and it is what
/// makes every `pos + size` in this module safe to write as a plain add: with
/// `size` bounded here and `pos` bounded by a buffer that had to be allocated,
/// the sum cannot wrap. Without it, a 64-bit `largesize` of `2^64 - 16` makes
/// `pos + size` wrap to *behind* the cursor: the reader spins forever on the
/// same bytes in release, and hands the merger a fragment whose mdat range
/// runs backwards. Both were found by review, both are one hostile box away.
///
/// `i32::MAX` rather than a round number, because a merged segment's `trun`
/// data offsets are signed 32-bit and the merger refuses anything larger
/// anyway — a box this reader accepted but the merger could not address would
/// be a failure deferred rather than avoided.
const MAX_BOX_BYTES: usize = i32::MAX as usize;

/// Largest `sample_count` a single `trun` may declare.
///
/// The length check below it is necessary but NOT sufficient: a `trun` that
/// sets none of the per-sample flags (legal — every value then comes from the
/// `tfhd`/`trex` defaults) has a per-sample cost of zero bytes, so an
/// eight-byte box can declare four billion samples and pass. `Vec` then tries
/// to reserve 64 GB, and a failed allocation is an abort, not an error: the
/// whole daemon dies rather than the one session falling back to ffmpeg's
/// muxer. Found by review, with a 76-byte `moof` that reproduces it.
///
/// A million samples is ~11 hours of 24 fps video or ~6 hours of AAC in ONE
/// fragment, against fragments that are one GOP each. Nothing real comes near
/// it, and the allocation it bounds is 16 MB.
const MAX_SAMPLES_PER_RUN: usize = 1 << 20;

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
    /// The sample entry carries a Dolby Vision decoder configuration (`dvcC`
    /// or `dvvC`). This is distinct from an `hvc1` base layer: ffmpeg may strip
    /// the Dolby configuration and RPUs while leaving the file-level `dby1`
    /// compatible brand behind.
    pub dolby_vision_config: bool,
    /// Bytes of the length prefix on each NAL unit inside a sample, from
    /// `hvcC`/`avcC`. Zero when the track has no such record.
    pub nal_length_size: u8,
    /// `trex` defaults — the last resort when neither `trun` nor `tfhd`
    /// carries a value. ffmpeg always fills `tfhd`, so these are insurance.
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}

/// `ftyp` + `moov`: the initialization segment, normally kept verbatim.
///
/// The one intentional exception is [`promote_hdr10_static_metadata`]. ffmpeg
/// can leave HDR10's mastering-display and content-light SEIs only in the
/// first media sample. Apple HLS requires those decoder-wide records in the
/// HEVC configuration carried by this segment, so the copy session may add
/// those exact NAL units to `hvcC` before publishing it. Sample data is never
/// rewritten.
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
    /// The track table from the `moov`, kept after the [`Init`] is handed out
    /// because the `trex` defaults on it are the bottom rung of every
    /// fragment's sample-resolution ladder. ffmpeg always fills `tfhd` and so
    /// never reaches them, which is exactly why they had to be wired up
    /// deliberately: a rung nothing exercises is a rung that quietly does not
    /// exist, and a stream that did lean on `trex` would have resolved every
    /// sample to zero duration — a segmenter that never reaches its floor and
    /// holds the whole film in memory.
    tracks: Vec<Track>,
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
                        self.tracks = tracks.clone();
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
                    let tracks = parse_moof(
                        &self.buf[self.pos..self.pos + hdr.size],
                        hdr.header_len,
                        &self.tracks,
                    )?;
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
            if large > MAX_BOX_BYTES as u64 {
                return Err(Fmp4Error::Unsupported(format!(
                    "box {} declares {large} bytes",
                    fourcc(&ty)
                )));
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
    if size > MAX_BOX_BYTES {
        return Err(Fmp4Error::Unsupported(format!(
            "box {} declares {size} bytes",
            fourcc(&ty)
        )));
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
        dolby_vision_config: false,
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
            // Guard rather than a nested `if`: a short hdlr falls through to
            // the `_ => {}` arm below, which is what the nested form did too.
            b"hdlr" if b.len() >= 12 => {
                track.kind = match &b[8..12] {
                    b"vide" => TrackKind::Video,
                    b"soun" => TrackKind::Audio,
                    _ => TrackKind::Other,
                };
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
                    (b"dvcC" | b"dvvC", _) => {
                        track.dolby_vision_config = true;
                    }
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
// HDR10 initialization metadata
// ---------------------------------------------------------------------------

/// Put HDR10's static metadata where an HLS decoder can see it before opening
/// the first media fragment.
///
/// ffmpeg's fragmented-MP4 copy path can carry mastering-display (SEI payload
/// type 137) and content-light-level (144) messages only in the first video
/// sample. That is enough for an elementary-stream decoder, but not for Apple
/// HLS: AVPlayer decides whether a `VIDEO-RANGE=PQ` variant is supported from
/// its initialization segment. Copy the original prefix-SEI NAL units into
/// the `hvcC` record, exactly as authored, and add Apple's recommended
/// `mdcv`/`clli` sample-entry boxes while leaving every media sample untouched.
///
/// Returns `true` when the init segment changed. Non-HEVC streams, streams
/// without the metadata, real Dolby Vision sample entries, and already-rich
/// `hvcC` records are no-ops.
pub fn promote_hdr10_static_metadata(init: &mut Init, first: &Fragment) -> Result<bool, Fmp4Error> {
    let Some(video) = init.video() else {
        return Ok(false);
    };
    if video.codec != Some(VideoCodec::Hevc)
        || video.dolby_vision_config
        || video.nal_length_size == 0
    {
        return Ok(false);
    }

    let Some(sample) = first_video_sample(first, video) else {
        return Ok(false);
    };
    let candidate_nals = hdr10_prefix_sei_nals(sample, video.nal_length_size);
    if candidate_nals.is_empty() {
        return Ok(false);
    }

    let Some(location) = locate_hvcc(&init.bytes)? else {
        return Err(Fmp4Error::Unsupported(
            "the HEVC video sample entry has no hvcC box to enrich".into(),
        ));
    };
    let record = &init.bytes[location.payload.clone()];
    let present = hvcc_hdr10_sei_types(record)?;
    let mut additions = Vec::new();
    let mut covered = present;
    let mut mastering = None;
    let mut content_light = None;
    for &nal in &candidate_nals {
        for (kind, payload) in hdr10_sei_messages(nal) {
            match kind {
                137 if mastering.is_none() => mastering = Some(payload),
                144 if content_light.is_none() => content_light = Some(payload),
                _ => {}
            }
        }
        let types = hdr10_sei_types(nal);
        if types.iter().all(|kind| covered.contains(kind)) {
            continue;
        }
        for kind in types {
            if !covered.contains(&kind) {
                covered.push(kind);
            }
        }
        additions.push(nal);
    }
    if additions.len() > u16::MAX as usize {
        return Err(Fmp4Error::Unsupported(
            "too many HDR10 prefix-SEI NAL units for hvcC".into(),
        ));
    }

    let mut changed = false;
    let mut hvcc_delta = 0usize;
    if !additions.is_empty() {
        let mut array = Vec::new();
        // array_completeness=0: these are the decoder-wide static records,
        // not a claim that no other per-picture prefix SEIs occur in samples.
        array.push(39);
        array.extend_from_slice(&(additions.len() as u16).to_be_bytes());
        for nal in additions {
            let len = u16::try_from(nal.len()).map_err(|_| {
                Fmp4Error::Unsupported("an HDR10 prefix-SEI NAL exceeds hvcC's u16 length".into())
            })?;
            array.extend_from_slice(&len.to_be_bytes());
            array.extend_from_slice(nal);
        }

        let arrays_offset = location.payload.start + 22;
        let arrays = init.bytes[arrays_offset];
        if arrays == u8::MAX {
            return Err(Fmp4Error::Unsupported(
                "hvcC already declares 255 NAL arrays".into(),
            ));
        }
        hvcc_delta = array.len();
        let insert_at = location.payload.end;
        init.bytes.splice(insert_at..insert_at, array);
        init.bytes[arrays_offset] = arrays + 1;
        for &box_at in &location.ancestors {
            grow_box(&mut init.bytes, box_at, hvcc_delta)?;
        }
        changed = true;
    }

    // Apple recommends explicit sample-entry atoms even though the same SEIs
    // in hvcC are a valid fallback. Carry both forms: older AVFoundation
    // versions have been observed consulting the atoms while deciding whether
    // a PQ HLS variant is supported, before they instantiate the decoder.
    let mut static_boxes = Vec::new();
    if !location.has_mdcv {
        if let Some(payload) = mastering.filter(|payload| payload.len() == 24) {
            static_boxes.extend_from_slice(&mp4_box(b"mdcv", &payload)?);
        }
    }
    if !location.has_clli {
        if let Some(payload) = content_light.filter(|payload| payload.len() == 4) {
            static_boxes.extend_from_slice(&mp4_box(b"clli", &payload)?);
        }
    }
    if !static_boxes.is_empty() {
        let delta = static_boxes.len();
        let insert_at = location.sample_entry_end + hvcc_delta;
        init.bytes.splice(insert_at..insert_at, static_boxes);
        // Skip hvcC itself; grow the visual sample entry and every container.
        for &box_at in &location.ancestors[1..] {
            grow_box(&mut init.bytes, box_at, delta)?;
        }
        changed = true;
    }
    Ok(changed)
}

fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>, Fmp4Error> {
    let size = payload
        .len()
        .checked_add(8)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| Fmp4Error::Unsupported("HDR10 metadata box exceeds u32".into()))?;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    Ok(out)
}

fn first_video_sample<'a>(fragment: &'a Fragment, video: &Track) -> Option<&'a [u8]> {
    let track = fragment.track(video.id)?;
    let run = track.runs.first()?;
    let sample = run.samples.first()?;
    let end = run.data_offset.checked_add(sample.size as usize)?;
    fragment.bytes.get(run.data_offset..end)
}

/// Prefix-SEI NALs from one length-prefixed HEVC sample that carry at least
/// one HDR10 static-metadata message.
fn hdr10_prefix_sei_nals(sample: &[u8], length_size: u8) -> Vec<&[u8]> {
    let lsz = length_size as usize;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while lsz > 0 && pos.checked_add(lsz).is_some_and(|end| end <= sample.len()) {
        let mut len = 0usize;
        for byte in &sample[pos..pos + lsz] {
            len = (len << 8) | *byte as usize;
        }
        let Some(start) = pos.checked_add(lsz) else {
            break;
        };
        let Some(end) = start.checked_add(len) else {
            break;
        };
        let Some(nal) = sample.get(start..end) else {
            break;
        };
        if nal.len() >= 2 && ((nal[0] >> 1) & 0x3f) == 39 && !hdr10_sei_types(nal).is_empty() {
            out.push(nal);
        }
        if len == 0 {
            break;
        }
        pos = end;
    }
    out
}

/// HDR10 static-metadata payload types carried by one HEVC SEI NAL.
fn hdr10_sei_types(nal: &[u8]) -> Vec<u16> {
    hdr10_sei_messages(nal)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

/// HDR10 messages and their unescaped big-endian payloads from one HEVC SEI
/// NAL. The payload bytes are exactly what the `mdcv`/`clli` boxes carry.
fn hdr10_sei_messages(nal: &[u8]) -> Vec<(u16, Vec<u8>)> {
    if nal.len() < 3 {
        return Vec::new();
    }
    // Remove emulation-prevention bytes before reading SEI payload headers.
    let mut rbsp = Vec::with_capacity(nal.len() - 2);
    let mut zeroes = 0u8;
    for &byte in &nal[2..] {
        if zeroes >= 2 && byte == 3 {
            continue;
        }
        rbsp.push(byte);
        zeroes = if byte == 0 {
            zeroes.saturating_add(1)
        } else {
            0
        };
    }

    let mut found = Vec::new();
    let mut pos = 0usize;
    while pos < rbsp.len() {
        // rbsp_trailing_bits, possibly followed by zero padding.
        if rbsp[pos] == 0x80 && rbsp[pos + 1..].iter().all(|byte| *byte == 0) {
            break;
        }
        let Some(kind) = sei_number(&rbsp, &mut pos) else {
            break;
        };
        let Some(size) = sei_number(&rbsp, &mut pos) else {
            break;
        };
        let Some(end) = pos.checked_add(size as usize) else {
            break;
        };
        if end > rbsp.len() {
            break;
        }
        if matches!(kind, 137 | 144) && !found.iter().any(|(seen, _)| *seen == kind) {
            found.push((kind, rbsp[pos..end].to_vec()));
        }
        pos = end;
    }
    found
}

fn sei_number(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    let mut value = 0u16;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value = value.checked_add(byte as u16)?;
        if byte != 0xff {
            return Some(value);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BoxAt {
    start: usize,
    header_len: usize,
}

struct HvcCLocation {
    payload: Range<usize>,
    sample_entry_end: usize,
    has_mdcv: bool,
    has_clli: bool,
    /// hvcC first, then every enclosing box through moov.
    ancestors: Vec<BoxAt>,
}

fn locate_hvcc(bytes: &[u8]) -> Result<Option<HvcCLocation>, Fmp4Error> {
    let Some((moov_at, moov)) = find_child(bytes, 0..bytes.len(), b"moov")? else {
        return Ok(None);
    };
    let moov_body = moov_at.start + moov.header_len..moov_at.start + moov.size;
    for (trak_at, trak) in find_children(bytes, moov_body, b"trak")? {
        let trak_body = trak_at.start + trak.header_len..trak_at.start + trak.size;
        let Some((mdia_at, mdia)) = find_child(bytes, trak_body, b"mdia")? else {
            continue;
        };
        let mdia_body = mdia_at.start + mdia.header_len..mdia_at.start + mdia.size;
        let Some((minf_at, minf)) = find_child(bytes, mdia_body, b"minf")? else {
            continue;
        };
        let minf_body = minf_at.start + minf.header_len..minf_at.start + minf.size;
        let Some((stbl_at, stbl)) = find_child(bytes, minf_body, b"stbl")? else {
            continue;
        };
        let stbl_body = stbl_at.start + stbl.header_len..stbl_at.start + stbl.size;
        let Some((stsd_at, stsd)) = find_child(bytes, stbl_body, b"stsd")? else {
            continue;
        };
        let entries_start = stsd_at.start + stsd.header_len + 8;
        let entries_end = stsd_at.start + stsd.size;
        if entries_start > entries_end {
            return malformed("stsd too short while locating hvcC");
        }
        for (entry_at, entry) in find_children(bytes, entries_start..entries_end, b"hvc1")?
            .into_iter()
            .chain(find_children(bytes, entries_start..entries_end, b"hev1")?)
            .chain(find_children(bytes, entries_start..entries_end, b"dvh1")?)
            .chain(find_children(bytes, entries_start..entries_end, b"dvhe")?)
        {
            let extra_start = entry_at.start + entry.header_len + 78;
            let extra_end = entry_at.start + entry.size;
            if extra_start > extra_end {
                return malformed("visual sample entry too short while locating hvcC");
            }
            let Some((hvcc_at, hvcc)) = find_child(bytes, extra_start..extra_end, b"hvcC")? else {
                continue;
            };
            let payload = hvcc_at.start + hvcc.header_len..hvcc_at.start + hvcc.size;
            if payload.len() < 23 {
                return malformed("hvcC too short for its NAL arrays");
            }
            return Ok(Some(HvcCLocation {
                payload,
                sample_entry_end: extra_end,
                has_mdcv: find_child(bytes, extra_start..extra_end, b"mdcv")?.is_some(),
                has_clli: find_child(bytes, extra_start..extra_end, b"clli")?.is_some(),
                ancestors: vec![
                    hvcc_at, entry_at, stsd_at, stbl_at, minf_at, mdia_at, trak_at, moov_at,
                ],
            }));
        }
    }
    Ok(None)
}

fn find_child(
    bytes: &[u8],
    range: Range<usize>,
    kind: &[u8; 4],
) -> Result<Option<(BoxAt, BoxHeader)>, Fmp4Error> {
    Ok(find_children(bytes, range, kind)?.into_iter().next())
}

fn find_children(
    bytes: &[u8],
    range: Range<usize>,
    kind: &[u8; 4],
) -> Result<Vec<(BoxAt, BoxHeader)>, Fmp4Error> {
    if range.end > bytes.len() || range.start > range.end {
        return malformed("child-box range runs outside its parent");
    }
    let mut found = Vec::new();
    let mut pos = range.start;
    while pos + 8 <= range.end {
        let Some(header) = peek_box(bytes, pos)? else {
            break;
        };
        let Some(end) = pos.checked_add(header.size) else {
            return malformed("box size overflows while locating hvcC");
        };
        if end > range.end {
            return malformed(format!(
                "box {} runs past its parent while locating hvcC",
                fourcc(header.kind())
            ));
        }
        if header.kind() == kind {
            found.push((
                BoxAt {
                    start: pos,
                    header_len: header.header_len,
                },
                header,
            ));
        }
        pos = end;
    }
    Ok(found)
}

fn hvcc_hdr10_sei_types(record: &[u8]) -> Result<Vec<u16>, Fmp4Error> {
    if record.len() < 23 {
        return malformed("hvcC too short for its NAL arrays");
    }
    let mut found = Vec::new();
    let mut pos = 23usize;
    for _ in 0..record[22] {
        if pos + 3 > record.len() {
            return malformed("hvcC NAL array header runs past the record");
        }
        let array_type = record[pos] & 0x3f;
        let count = u16::from_be_bytes([record[pos + 1], record[pos + 2]]) as usize;
        pos += 3;
        for _ in 0..count {
            if pos + 2 > record.len() {
                return malformed("hvcC NAL length runs past the record");
            }
            let len = u16::from_be_bytes([record[pos], record[pos + 1]]) as usize;
            pos += 2;
            let Some(end) = pos.checked_add(len) else {
                return malformed("hvcC NAL length overflows");
            };
            if end > record.len() {
                return malformed("hvcC NAL runs past the record");
            }
            if array_type == 39 {
                for kind in hdr10_sei_types(&record[pos..end]) {
                    if !found.contains(&kind) {
                        found.push(kind);
                    }
                }
            }
            pos = end;
        }
    }
    if pos != record.len() {
        return malformed("hvcC has trailing bytes after its NAL arrays");
    }
    Ok(found)
}

fn grow_box(bytes: &mut [u8], at: BoxAt, delta: usize) -> Result<(), Fmp4Error> {
    match at.header_len {
        8 => {
            let old = be_u32(bytes, at.start) as usize;
            let new = old
                .checked_add(delta)
                .and_then(|size| u32::try_from(size).ok())
                .ok_or_else(|| {
                    Fmp4Error::Unsupported("box size exceeds u32 after hvcC enrichment".into())
                })?;
            bytes[at.start..at.start + 4].copy_from_slice(&new.to_be_bytes());
        }
        16 => {
            let old = be_u64(bytes, at.start + 8);
            let new = old.checked_add(delta as u64).ok_or_else(|| {
                Fmp4Error::Unsupported("extended box size overflows after hvcC enrichment".into())
            })?;
            bytes[at.start + 8..at.start + 16].copy_from_slice(&new.to_be_bytes());
        }
        other => {
            return Err(Fmp4Error::Unsupported(format!(
                "cannot grow a box with a {other}-byte header"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// moof
// ---------------------------------------------------------------------------

/// Parse a `moof`'s track fragments. Takes the whole box, header included,
/// because every `trun` data offset is relative to its first byte.
fn parse_moof(
    moof: &[u8],
    header_len: usize,
    tracks: &[Track],
) -> Result<Vec<TrackFragment>, Fmp4Error> {
    let mut out = Vec::new();
    let body = &moof[header_len..];
    for (hdr, start, end) in children(body)? {
        if hdr.kind() != b"traf" {
            continue;
        }
        out.push(parse_traf(&body[start..end], tracks)?);
    }
    if out.is_empty() {
        return malformed("moof has no traf");
    }
    Ok(out)
}

fn parse_traf(payload: &[u8], tracks: &[Track]) -> Result<TrackFragment, Fmp4Error> {
    let mut track_id = 0u32;
    let mut base_decode_time = 0u64;
    // Seeded from `trex`, overwritten by `tfhd`, overwritten again per sample
    // by `trun` — the ladder ISO 14496-12 §8.8.7 describes, bottom rung first.
    // Filled once the `tfhd` names the track, since which `trex` applies is
    // not known before that.
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
                if let Some(t) = tracks.iter().find(|t| t.id == track_id) {
                    default_duration = t.default_sample_duration;
                    default_size = t.default_sample_size;
                    default_flags = t.default_sample_flags;
                }
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

    // Both checks are needed. The length check catches a `trun` whose declared
    // samples run past the box; the count cap catches one whose per-sample
    // cost is zero (no per-sample flags set), where any `sample_count`
    // survives the length check and the allocation below is what dies.
    if count > MAX_SAMPLES_PER_RUN {
        return malformed(format!("trun declares {count} samples"));
    }
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
    /// The same, for the FIRST segment only, and much lower.
    ///
    /// A player's runway at startup is exactly the first segment, because that
    /// is all the playlist holds; its next chance to learn any other segment
    /// exists is one `TARGETDURATION` away. So the first segment has to be
    /// short enough that the first reload lands with time to spare — and by
    /// then the burst has filled the playlist and the question never arises
    /// again. It also puts the first frame on screen sooner, since nothing
    /// plays until one whole segment exists.
    ///
    /// The cost is one extra boundary per session, and usually not even that:
    /// the cut still has to land on a clean keyframe, and on a source with
    /// clean points every couple of seconds it will.
    pub first_floor_ticks: u64,
    /// Hard ceiling in bytes. Binding at 69 Mb/s long before any duration is.
    pub max_bytes: usize,
    /// Secondary ceiling in ticks, for sources whose bytes never pile up.
    pub max_ticks: u64,
}

impl CutPolicy {
    /// Build from the shipped constants and the stream's own video timescale.
    pub fn new(
        floor_seconds: u32,
        first_floor_seconds: u32,
        max_bytes: usize,
        max_seconds: u32,
        timescale: u32,
    ) -> CutPolicy {
        let ts = timescale.max(1) as u64;
        CutPolicy {
            floor_ticks: floor_seconds as u64 * ts,
            first_floor_ticks: first_floor_seconds.min(floor_seconds) as u64 * ts,
            max_bytes,
            max_ticks: max_seconds as u64 * ts,
        }
    }

    /// Should the fragments accumulated so far be published *before* the
    /// fragment that just arrived? `None` means keep accumulating.
    ///
    /// **The floor gates everything, including the ceilings**, and that order
    /// is load-bearing rather than tidy. `COPY_SEGMENT_MAX_BYTES` is 48 MB and
    /// the reference 4K remux runs at 69 Mb/s, which reaches 48 MB in 5.6
    /// seconds — so a ceiling allowed to fire on its own would cut *below* the
    /// six-second floor and produce MORE boundaries than the muxer this
    /// replaces, on exactly the file the whole investigation is about. Every
    /// boundary costs a frame; a segmenter that adds boundaries to a source
    /// with no clean point would be worse than doing nothing, which is the one
    /// thing this is not allowed to be. The floor is the measured decision
    /// (`e212c55`, which weighed 6 s ≈ 52 MB at 69 Mb/s and took it); the
    /// ceilings bound how long we are willing to wait *past* it for a clean
    /// point, and nothing else.
    ///
    /// After the floor, a clean cut wins over both ceilings — so a boundary
    /// that could be clean is never spent as a ceiling cut, and a ceiling that
    /// happens to land in front of a clean fragment is counted clean, because
    /// the ceiling decided only *when*, not *where*.
    pub fn cut_before(
        &self,
        pending_ticks: u64,
        pending_bytes: usize,
        next: CutClass,
        first_segment: bool,
    ) -> Option<CutReason> {
        let floor = if first_segment {
            self.first_floor_ticks
        } else {
            self.floor_ticks
        };
        if pending_bytes == 0 || pending_ticks < floor {
            return None;
        }
        if next.is_clean() {
            return Some(CutReason::Clean);
        }
        if pending_bytes >= self.max_bytes {
            return Some(CutReason::ByteCeiling);
        }
        if pending_ticks >= self.max_ticks {
            return Some(CutReason::TimeCeiling);
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

/// A contiguous stretch of one track's sample data inside one source fragment.
/// Copied whole into the merged `mdat`; every sample in it keeps its bytes.
struct Slice {
    /// Index into the merged fragment list.
    fragment: usize,
    /// Byte range within that fragment's own bytes.
    start: usize,
    len: usize,
}

/// One track's whole contribution to the merged segment: every sample in
/// order, and the byte slices they live in.
struct Plan {
    track_id: u32,
    base_decode_time: u64,
    samples: Vec<Sample>,
    slices: Vec<Slice>,
}

impl Plan {
    fn byte_len(&self) -> usize {
        self.slices.iter().map(|s| s.len).sum()
    }
}

/// Merge consecutive fragments into one `styp moof mdat` HLS segment.
///
/// **The shape is ffmpeg's HLS muxer's shape, deliberately** — `styp`, one
/// `sidx` per track, `moof` with one `traf` and one `trun` per track, `mdat`.
///
/// The first cut of this diverged twice: it omitted the `sidx` boxes (optional
/// in HLS fMP4, and hls.js's passthrough ignores them) and it wrote one `trun`
/// per source fragment rather than one per track, because that let each source
/// `mdat` payload be copied whole. Chrome did not care about either. Safari
/// refused the stream outright and the player's error fallback re-encoded a 4K
/// remux down to 1080p — the exact failure this path exists to prevent, on the
/// exact browser it exists for.
///
/// So the divergence is gone. Each track's slices are copied into the merged
/// `mdat` consecutively, which makes its whole contribution contiguous and one
/// data offset enough for it; the `sidx` boxes are byte-for-byte the shape
/// ffmpeg writes, `earliest_presentation_time` included. The bytes a player
/// sees now differ from the muxer this replaced only in *where the boundaries
/// fall*, which was always the entire point.
///
/// The one thing still normalized rather than copied: `tfhd` carries only
/// `default-base-is-moof` and every sample writes its own duration, size,
/// flags and composition offset, where ffmpeg leans on `tfhd` defaults. That
/// is a density choice, not a structural one — the fully-explicit form is the
/// most-exercised path in every MP4 parser there is — and it keeps the
/// default-value and first-sample-flags ladders confined to the reader.
pub fn merge(fragments: &[Fragment], init: &Init, sequence: u32) -> Result<Segment, Fmp4Error> {
    if fragments.is_empty() {
        return malformed("nothing to merge");
    }
    let mut stats = MergeStats::default();

    // The UNION of the tracks in the run, in first-seen order — which is
    // ffmpeg's own interleave order, video first, so a diff against hlsenc's
    // output stays readable.
    //
    // The union, and not `fragments[0]`'s tracks, because ffmpeg does not
    // guarantee every fragment carries every track: `mov_flush_fragment`
    // writes a `traf` only for tracks with buffered samples, and once a track
    // goes `max_interleave_delta` (10 s) without a packet the muxer stops
    // waiting for it. A source with a late-starting or gapped audio track
    // therefore produces video-only fragments followed by fragments that
    // carry audio again — and taking the track list from the first of those
    // published a SILENT segment whose audio bytes were still copied into the
    // mdat, with no error and no counter anywhere. Found by review.
    let mut track_ids: Vec<u32> = Vec::new();
    for frag in fragments {
        for t in &frag.tracks {
            if !track_ids.contains(&t.track_id) {
                track_ids.push(t.track_id);
            }
        }
    }
    let mut plans: Vec<Plan> = Vec::with_capacity(track_ids.len());

    for &id in &track_ids {
        let mut samples: Vec<Sample> = Vec::new();
        let mut slices: Vec<Slice> = Vec::new();
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
                            if let Some(last) = samples.last_mut() {
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
                samples.extend_from_slice(&run.samples);
                slices.push(Slice {
                    fragment: fi,
                    start: run.data_offset,
                    len: run.byte_len(),
                });
            }
            expected_next = Some(tf.base_decode_time + dur);
        }
        if let (Some(base), false) = (base, samples.is_empty()) {
            plans.push(Plan {
                track_id: id,
                base_decode_time: base,
                samples,
                slices,
            });
        }
    }
    if plans.is_empty() {
        return malformed("no track survived the merge");
    }

    // Build the moof with zeroed data offsets to learn its size. Every field
    // that could change the size is fixed-width, so the patch below only
    // changes values — the layout is already final.
    let mut offset_slots: Vec<usize> = Vec::new();
    let mut moof = build_moof(&plans, sequence, &mut offset_slots);
    let moof_size = moof.len();

    let payload_total: usize = plans.iter().map(|p| p.byte_len()).sum();
    let Some(mdat_size) = payload_total.checked_add(8) else {
        return malformed("merged mdat size overflows");
    };
    if mdat_size > u32::MAX as usize {
        return Err(Fmp4Error::Unsupported(format!(
            "merged mdat would be {mdat_size} bytes"
        )));
    }

    // Each track's data is contiguous in the merged mdat, which is what lets
    // its whole contribution be one `trun` with one data offset.
    let mut base = 0usize;
    for (i, plan) in plans.iter().enumerate() {
        let offset = moof_size + 8 + base;
        if offset > i32::MAX as usize {
            return Err(Fmp4Error::Unsupported(
                "merged segment exceeds a 32-bit trun data offset".into(),
            ));
        }
        let pos = offset_slots[i];
        moof[pos..pos + 4].copy_from_slice(&(offset as u32).to_be_bytes());
        base += plan.byte_len();
    }

    let sidx_total = plans.len() * SIDX_BYTES;
    let referenced_size = moof_size + mdat_size;
    let mut out = Vec::with_capacity(STYP.len() + sidx_total + referenced_size);
    out.extend_from_slice(&STYP);
    for (i, plan) in plans.iter().enumerate() {
        let timescale = init
            .track(plan.track_id)
            .map(|t| t.timescale)
            .unwrap_or(1)
            .max(1);
        let duration: u64 = plan.samples.iter().map(|s| s.duration as u64).sum();
        // Every sidx after this one sits between it and the material it
        // describes, which is what `first_offset` measures.
        let first_offset = ((plans.len() - 1 - i) * SIDX_BYTES) as u64;
        write_sidx(
            &mut out,
            plan.track_id,
            timescale,
            plan.base_decode_time,
            first_offset,
            referenced_size as u32,
            duration.min(u32::MAX as u64) as u32,
        );
    }
    out.extend_from_slice(&moof);
    out.extend_from_slice(&(mdat_size as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for plan in &plans {
        for slice in &plan.slices {
            let frag = &fragments[slice.fragment];
            let end = slice.start.saturating_add(slice.len);
            if end > frag.bytes.len() {
                return malformed("a run points past the end of its fragment");
            }
            out.extend_from_slice(&frag.bytes[slice.start..end]);
        }
    }

    let video_ticks = init
        .video()
        .and_then(|v| plans.iter().find(|p| p.track_id == v.id))
        .map(|p| p.samples.iter().map(|s| s.duration as u64).sum())
        .unwrap_or(0);

    Ok(Segment {
        bytes: out,
        stats,
        video_ticks,
    })
}

/// A version-1 `sidx` is 52 bytes: the box header, the full-box header, two
/// 32-bit ids, two 64-bit times, the reserved/count pair, and one 12-byte
/// reference.
const SIDX_BYTES: usize = 52;

/// One `sidx`, in exactly the shape ffmpeg's HLS muxer writes — version 1,
/// one reference covering the whole `moof`+`mdat`, `earliest_presentation_time`
/// set to the track's `tfdt` (which is what ffmpeg puts there; it is a decode
/// time, not the true earliest presentation time, and every player that has
/// ever consumed this path has consumed that).
///
/// `starts_with_SAP` is 1 with `SAP_type` 0, again copying ffmpeg rather than
/// getting clever: a segment this module publishes starts on a random-access
/// point by construction on the clean path, and on a ceiling cut it starts on
/// an IRAP whose leading pictures a player discards — the same thing ffmpeg's
/// own segments have always declared here.
fn write_sidx(
    out: &mut Vec<u8>,
    track_id: u32,
    timescale: u32,
    earliest_presentation_time: u64,
    first_offset: u64,
    referenced_size: u32,
    subsegment_duration: u32,
) {
    out.extend_from_slice(&(SIDX_BYTES as u32).to_be_bytes());
    out.extend_from_slice(b"sidx");
    out.extend_from_slice(&0x0100_0000u32.to_be_bytes()); // version 1, flags 0
    out.extend_from_slice(&track_id.to_be_bytes());
    out.extend_from_slice(&timescale.to_be_bytes());
    out.extend_from_slice(&earliest_presentation_time.to_be_bytes());
    out.extend_from_slice(&first_offset.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // reserved
    out.extend_from_slice(&1u16.to_be_bytes()); // reference_count
                                                // reference_type 0 (media) | referenced_size
    out.extend_from_slice(&(referenced_size & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(&subsegment_duration.to_be_bytes());
    // starts_with_SAP 1 | SAP_type 0 | SAP_delta_time 0
    out.extend_from_slice(&0x8000_0000u32.to_be_bytes());
}

/// The 24-byte `styp` hlsenc writes, copied exactly: major brand `msdh`,
/// compatible with `msdh` and `msix`.
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

/// A `trun` is version 1 exactly when it has to be: version 0 composition
/// offsets are UNSIGNED.
///
/// ffmpeg shifts dts to keep every offset non-negative unless it is asked for
/// `+negative_cts_offsets`, which plurx does not pass — so in production this
/// is always 0. It matters anyway, because the alternative was writing
/// `cto.max(0)` into a version-0 field: a negative offset would then be
/// silently clamped, shifting that frame's presentation time by the offset it
/// lost, with no error — which is exactly the class of change the framemd5
/// equality exists to forbid, arriving without tripping it. Found by review.
fn trun_version(samples: &[Sample]) -> u8 {
    u8::from(samples.iter().any(|s| s.cto < 0))
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

        // ONE trun per traf. Not a style choice: ffmpeg's HLS muxer writes
        // exactly one, and a segment carrying seven of them is a shape Safari
        // has never been asked to read on this path. Getting there costs
        // nothing — each track's slices are copied into the merged mdat
        // consecutively, so its whole contribution is contiguous and one data
        // offset addresses all of it.
        let flags = trun_flags(&plan.samples);
        let version = trun_version(&plan.samples);
        let has_cto = flags & 0x0000_0800 != 0;
        let per = if has_cto { 16 } else { 12 };
        let size = 8 + 4 + 4 + 4 + plan.samples.len() * per;
        moof.extend_from_slice(&(size as u32).to_be_bytes());
        moof.extend_from_slice(b"trun");
        moof.extend_from_slice(&(((version as u32) << 24) | flags).to_be_bytes());
        moof.extend_from_slice(&(plan.samples.len() as u32).to_be_bytes());
        offset_slots.push(moof.len());
        moof.extend_from_slice(&0u32.to_be_bytes()); // data_offset, patched
        for s in &plan.samples {
            moof.extend_from_slice(&s.duration.to_be_bytes());
            moof.extend_from_slice(&s.size.to_be_bytes());
            moof.extend_from_slice(&s.flags.to_be_bytes());
            if has_cto {
                // Written in whichever width the values need, and clamped in
                // neither: version 0 is unsigned, version 1 signed.
                let clamped = s.cto.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                moof.extend_from_slice(&clamped.to_be_bytes());
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
    /// `EXTINF`, normally from summed video sample durations in the video
    /// timescale. The final segment uses its longest track when audio outlasts
    /// the last video frame.
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

    /// How long the pending run runs, in seconds — the number that becomes
    /// `EXTINF`.
    ///
    /// The video track's summed sample durations in the video timescale, per
    /// STUTTER-4K §6.3: audio fragment durations differ from video's by up to
    /// one AAC frame, and using them drifts the playlist against the media
    /// over a film.
    ///
    /// At end of stream the longest track is authoritative. ffmpeg may put the
    /// last video frame and a much longer audio tail in the same fragment, so
    /// falling back only when video duration is exactly zero truncates that
    /// mixed final segment in the playlist. Ordinary cuts still use video: an
    /// audio frame of per-fragment drift must not accumulate across the film.
    fn pending_seconds(&self, reason: CutReason) -> f64 {
        let video_ticks: u64 = self
            .pending
            .iter()
            .map(|f| f.video_duration(&self.init))
            .sum();
        let video_seconds = video_ticks as f64 / self.video_timescale as f64;
        if video_ticks > 0 && reason != CutReason::EndOfStream {
            return video_seconds;
        }
        let mut longest = video_seconds;
        for track in &self.init.tracks {
            let ticks: u64 = self
                .pending
                .iter()
                .filter_map(|f| f.track(track.id))
                .map(|t| t.duration())
                .sum();
            let secs = ticks as f64 / track.timescale.max(1) as f64;
            if secs > longest {
                longest = secs;
            }
        }
        longest
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
        let published = match self.policy.cut_before(
            self.pending_ticks,
            self.pending_bytes,
            class,
            self.next_index == 0,
        ) {
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
        let seconds = self.pending_seconds(reason);
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

/// The playlist header, rewritten with the playlist on every segment.
///
/// **`TARGETDURATION` is `ceil` of the longest segment published so far, and
/// it only ever grows.** SEGMENTER-PLAN §3 said to declare the duration
/// ceiling (15) up front instead, so the tag could never decrease. The
/// reasoning was right and the consequence was not: on a live EVENT playlist
/// this tag *is* the client's reload interval (RFC 8216 §6.3.4), so declaring
/// 15 told Safari to wait fifteen seconds between playlist fetches. It loaded
/// a playlist holding one 9.2-second segment, played it out, and then sat with
/// nothing to play for the remaining 5.8 seconds — measured at 5631 ms, once
/// per film, always at the same position, on a server that was 55 seconds
/// ahead the whole time.
///
/// ffmpeg's HLS muxer grows this tag, which is why the path never had the
/// problem before. Growing is what a live playlist needs; the invariant that
/// matters is that it never *shrinks*, and this never does.
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

    use crate::testfixtures::{ffmpeg, ffprobe, pipe, pipe_path, run};

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
        assert!(!video.dolby_vision_config);
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

    fn length_prefixed_hevc_nals(nals: &[&[u8]]) -> Vec<u8> {
        let mut sample = Vec::new();
        for nal in nals {
            sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            sample.extend_from_slice(nal);
        }
        sample
    }

    fn fragment_with_first_video_sample(track_id: u32, sample: Vec<u8>) -> Fragment {
        let size = sample.len() as u32;
        Fragment {
            mdat_payload: 0..sample.len(),
            bytes: sample,
            tracks: vec![TrackFragment {
                track_id,
                base_decode_time: 0,
                runs: vec![Run {
                    data_offset: 0,
                    samples: vec![Sample {
                        duration: 1_000,
                        size,
                        flags: 0,
                        cto: 0,
                    }],
                }],
            }],
        }
    }

    #[test]
    fn hdr10_static_metadata_is_promoted_into_hvcc_once() {
        let feed = pipe("open-gop");
        let (mut init, _, _) = read_all(&feed);
        let video = init.video().expect("HEVC video").clone();
        // HEVC prefix-SEI NALs with the exact payload lengths standardized for
        // the mdcv (24 bytes) and clli (4 bytes) sample-entry boxes.
        let mut mastering = vec![0x4e, 0x01, 137, 24];
        mastering.extend_from_slice(&[0; 24]);
        mastering.push(0x80);
        let mut content_light = vec![0x4e, 0x01, 144, 4];
        content_light.extend_from_slice(&[0; 4]);
        content_light.push(0x80);
        let vcl = [0x26, 0x01, 0x80];
        let fragment = fragment_with_first_video_sample(
            video.id,
            length_prefixed_hevc_nals(&[&mastering, &content_light, &vcl]),
        );

        let original_len = init.bytes.len();
        assert!(promote_hdr10_static_metadata(&mut init, &fragment).expect("promotion"));
        assert!(init.bytes.len() > original_len);

        let location = locate_hvcc(&init.bytes)
            .expect("locating enriched hvcC")
            .expect("hvcC");
        let mut types =
            hvcc_hdr10_sei_types(&init.bytes[location.payload]).expect("reading enriched hvcC");
        types.sort_unstable();
        assert_eq!(types, vec![137, 144]);
        assert!(location.has_mdcv, "the recommended mdcv box is absent");
        assert!(location.has_clli, "the recommended clli box is absent");

        // Every enclosing box size was grown consistently: the ordinary
        // reader must still accept the complete initialization segment.
        let mut reader = FragmentReader::new();
        reader.push(&init.bytes);
        assert!(matches!(
            reader.next_unit().expect("re-parsing enriched init"),
            Some(Unit::Init(_))
        ));
        assert_eq!(reader.buffered(), 0);

        let once = init.bytes.clone();
        assert!(!promote_hdr10_static_metadata(&mut init, &fragment).expect("second promotion"));
        assert_eq!(init.bytes, once, "metadata was duplicated in hvcC");
    }

    #[test]
    fn a_first_sample_without_hdr10_static_metadata_leaves_init_verbatim() {
        let feed = pipe("open-gop");
        let (mut init, _, _) = read_all(&feed);
        let video = init.video().expect("HEVC video").clone();
        let unrelated_sei = [0x4e, 0x01, 5, 1, 0, 0x80];
        let vcl = [0x26, 0x01, 0x80];
        let fragment = fragment_with_first_video_sample(
            video.id,
            length_prefixed_hevc_nals(&[&unrelated_sei, &vcl]),
        );
        let original = init.bytes.clone();

        assert!(!promote_hdr10_static_metadata(&mut init, &fragment).expect("no-op promotion"));
        assert_eq!(init.bytes, original);
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
        let path = pipe_path("open-gop");
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
        let p = CutPolicy::new(6, 2, 48_000_000, 15, 1_000);
        // Under the floor, clean or not: keep going.
        assert_eq!(p.cut_before(5_000, 1_000, CutClass::CleanIdr, false), None);
        // Past the floor but the next fragment is dirty: still keep going,
        // which is the entire behaviour change this module ships.
        assert_eq!(p.cut_before(7_000, 1_000, CutClass::Dirty, false), None);
        assert_eq!(
            p.cut_before(7_000, 1_000, CutClass::Unparseable, false),
            None
        );
        // Past the floor with a clean fragment in front: cut.
        assert_eq!(
            p.cut_before(7_000, 1_000, CutClass::CleanCra, false),
            Some(CutReason::Clean)
        );
        // Nothing pending is not a cut, however clean the next fragment is.
        assert_eq!(p.cut_before(0, 0, CutClass::CleanIdr, false), None);
    }

    /// The trap this ordering exists for, in one assertion. 48 MB arrives in
    /// 5.6 s at the reference file's 69 Mb/s — a ceiling allowed to fire on
    /// its own would cut below the six-second floor and hand the viewer MORE
    /// boundaries than ffmpeg's muxer does, on the exact file this was built
    /// for. Every boundary costs a frame.
    #[test]
    fn no_ceiling_may_cut_below_the_floor() {
        let p = CutPolicy::new(6, 2, 48_000_000, 15, 1_000);
        assert_eq!(
            p.cut_before(5_600, 48_000_000, CutClass::Dirty, false),
            None
        );
        assert_eq!(
            p.cut_before(5_999, usize::MAX, CutClass::Dirty, false),
            None
        );
        // Not even for a clean fragment: under the floor there is nothing to
        // publish yet, and waiting costs nothing but a longer segment.
        assert_eq!(
            p.cut_before(5_999, 48_000_000, CutClass::CleanIdr, false),
            None
        );
        // One tick past it, the ceiling is free to act.
        assert_eq!(
            p.cut_before(6_000, 48_000_000, CutClass::Dirty, false),
            Some(CutReason::ByteCeiling)
        );
    }

    #[test]
    fn the_ceilings_cut_when_no_clean_point_arrives() {
        let p = CutPolicy::new(6, 2, 48_000_000, 15, 1_000);
        assert_eq!(
            p.cut_before(7_000, 48_000_000, CutClass::Dirty, false),
            Some(CutReason::ByteCeiling)
        );
        assert_eq!(
            p.cut_before(15_000, 1_000, CutClass::Dirty, false),
            Some(CutReason::TimeCeiling)
        );
        // A ceiling that happens to land in front of a clean fragment is a
        // clean cut: the ceiling decided when, not where.
        assert_eq!(
            p.cut_before(15_000, 48_000_000, CutClass::CleanIdr, false),
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
        let policy = CutPolicy::new(floor_s, floor_s, max_bytes, max_s, timescale);
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
        let reference = pipe_path("open-gop");
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

        // And the playlist matches the complete video timeline plus only the
        // final segment's legitimate trailing-media extension. Earlier
        // segments stay pinned to video so per-fragment audio drift cannot
        // accumulate across the film.
        let video = init.video().expect("video");
        let claimed: f64 = published.iter().map(|p| p.seconds).sum();
        let actual_video = end.get(&video.id).copied().unwrap_or(0) as f64 / video.timescale as f64;
        let final_segment = published.last().expect("a final segment");
        let final_video = final_segment.segment.video_ticks as f64 / video.timescale.max(1) as f64;
        let actual = actual_video + (final_segment.seconds - final_video).max(0.0);
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
        let reference = pipe_path("open-gop");

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

    /// A published segment has ffmpeg's HLS muxer's shape, box for box.
    ///
    /// This is not tidiness. The first cut of the merger omitted the `sidx`
    /// boxes and wrote one `trun` per source fragment; Chrome played it and
    /// Safari refused the stream, and the player's error fallback re-encoded
    /// a 4K remux down to 1080p. The shape is load-bearing, so it is asserted.
    #[test]
    fn a_published_segment_has_the_muxers_shape() {
        let (init, published, _) = segment("open-gop", 3, 300_000, 15);
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
        assert_eq!(types, vec!["styp", "sidx", "sidx", "moof", "mdat"]);
        assert_eq!(&bytes[..24], &STYP, "styp is not hlsenc's 24 bytes");

        // One traf per track, and inside it exactly one trun.
        let mut feed = init.bytes.clone();
        feed.extend_from_slice(bytes);
        let (_, frags, _) = read_all(&feed);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].tracks.len(), 2);
        for t in &frags[0].tracks {
            assert_eq!(
                t.runs.len(),
                1,
                "track {} published {} truns; ffmpeg writes one",
                t.track_id,
                t.runs.len()
            );
        }
    }

    /// The `sidx` boxes say what ffmpeg's say: version 1, one reference
    /// covering the whole moof+mdat, `earliest_presentation_time` equal to the
    /// track's `tfdt`, and a `first_offset` that steps over the sidx boxes
    /// still between it and the material.
    #[test]
    fn the_segment_index_describes_the_segment_it_precedes() {
        let (init, published, _) = segment("open-gop", 3, 300_000, 15);
        let seg = &published[0].segment.bytes;
        let mut feed = init.bytes.clone();
        feed.extend_from_slice(seg);
        let (_, frags, _) = read_all(&feed);

        let mut p = STYP.len();
        let mut seen = 0usize;
        let mut sidxes = Vec::new();
        while let Ok(Some(hdr)) = peek_box(seg, p) {
            if hdr.kind() != b"sidx" {
                break;
            }
            let b = &seg[p..p + hdr.size];
            assert_eq!(hdr.size, 52, "not a version-1 sidx");
            assert_eq!(b[8], 1, "sidx version");
            sidxes.push((
                be_u32(b, 12),               // reference_ID
                be_u32(b, 16),               // timescale
                be_u64(b, 20),               // earliest_presentation_time
                be_u64(b, 28),               // first_offset
                be_u32(b, 40) & 0x7fff_ffff, // referenced_size
                be_u32(b, 44),               // subsegment_duration
                be_u32(b, 48) >> 31,         // starts_with_SAP
            ));
            p += hdr.size;
            seen += 1;
        }
        assert_eq!(seen, frags[0].tracks.len(), "one sidx per track");

        let after_sidx = STYP.len() + seen * 52;
        let referenced = seg.len() - after_sidx;
        for (i, sx) in sidxes.iter().enumerate() {
            let tf = frags[0]
                .track(sx.0)
                .unwrap_or_else(|| panic!("sidx names track {} which is absent", sx.0));
            let track = init.track(sx.0).expect("track in the init");
            assert_eq!(sx.1, track.timescale, "sidx timescale");
            assert_eq!(
                sx.2, tf.base_decode_time,
                "earliest_presentation_time != tfdt"
            );
            assert_eq!(
                sx.3,
                ((seen - 1 - i) * 52) as u64,
                "first_offset does not step over the remaining sidx boxes"
            );
            assert_eq!(sx.4 as usize, referenced, "referenced_size != moof+mdat");
            assert_eq!(sx.5 as u64, tf.duration(), "subsegment_duration");
            assert_eq!(sx.6, 1, "starts_with_SAP");
        }
    }

    // -----------------------------------------------------------------------
    // hostile input — every one of these was found by review, not by a fixture
    // -----------------------------------------------------------------------

    /// Build a minimal `moof`+`mdat` by hand, so the reader can be shown bytes
    /// no ffmpeg would ever write.
    fn hand_built_fragment(trun_flags: u32, sample_count: u32, samples: &[u8]) -> Vec<u8> {
        let mut trun = Vec::new();
        trun.extend_from_slice(&trun_flags.to_be_bytes());
        trun.extend_from_slice(&sample_count.to_be_bytes());
        trun.extend_from_slice(samples);
        let mut traf = Vec::new();
        traf.extend_from_slice(&16u32.to_be_bytes());
        traf.extend_from_slice(b"tfhd");
        traf.extend_from_slice(&0x0002_0000u32.to_be_bytes());
        traf.extend_from_slice(&1u32.to_be_bytes());
        traf.extend_from_slice(&20u32.to_be_bytes());
        traf.extend_from_slice(b"tfdt");
        traf.extend_from_slice(&0x0100_0000u32.to_be_bytes());
        traf.extend_from_slice(&0u64.to_be_bytes());
        traf.extend_from_slice(&((trun.len() + 8) as u32).to_be_bytes());
        traf.extend_from_slice(b"trun");
        traf.extend_from_slice(&trun);

        let mut moof = Vec::new();
        moof.extend_from_slice(&((traf.len() + 8 + 16 + 8) as u32).to_be_bytes());
        moof.extend_from_slice(b"moof");
        moof.extend_from_slice(&16u32.to_be_bytes());
        moof.extend_from_slice(b"mfhd");
        moof.extend_from_slice(&0u32.to_be_bytes());
        moof.extend_from_slice(&1u32.to_be_bytes());
        moof.extend_from_slice(&((traf.len() + 8) as u32).to_be_bytes());
        moof.extend_from_slice(b"traf");
        moof.extend_from_slice(&traf);
        moof.extend_from_slice(&8u32.to_be_bytes());
        moof.extend_from_slice(b"mdat");
        moof
    }

    /// A `trun` that sets no per-sample flags has a per-sample cost of ZERO
    /// bytes, so the "declares samples it does not carry" length check passes
    /// for any `sample_count` — and `Vec::with_capacity` then asks the
    /// allocator for 64 GB. A failed allocation is an abort, not an error:
    /// the whole daemon dies rather than one session falling back to ffmpeg's
    /// muxer. This is a 76-byte input.
    #[test]
    fn a_trun_cannot_declare_four_billion_samples() {
        // flags = data-offset only; count = u32::MAX; no per-sample bytes.
        let frag = hand_built_fragment(0x0000_0001, u32::MAX, &0u32.to_be_bytes());
        assert!(frag.len() < 200, "the whole attack is {} bytes", frag.len());
        let feed = pipe("open-gop");
        let (init, _, _) = read_all(&feed);
        let mut reader = FragmentReader::new();
        reader.push(&init.bytes); // a real init, so the fragment path is reached
        assert!(matches!(reader.next_unit(), Ok(Some(Unit::Init(_)))));
        reader.push(&frag);
        assert!(
            matches!(reader.next_unit(), Err(Fmp4Error::Malformed(_))),
            "a four-billion-sample trun was accepted"
        );
    }

    /// A 64-bit `largesize` near `u64::MAX` makes `pos + size` wrap to behind
    /// the cursor. Two things followed: the reader spun forever on the same
    /// bytes in release, and the merger got a fragment whose mdat range ran
    /// backwards and panicked slicing it. One bound fixes both.
    #[test]
    fn a_box_that_would_wrap_the_cursor_is_refused() {
        let mut feed = Vec::new();
        feed.extend_from_slice(&16u32.to_be_bytes());
        feed.extend_from_slice(b"free");
        feed.extend_from_slice(&[0u8; 8]);
        // size==1 → 64-bit largesize, chosen so `16 + size` wraps to 0.
        feed.extend_from_slice(&1u32.to_be_bytes());
        feed.extend_from_slice(b"skip");
        feed.extend_from_slice(&(u64::MAX - 15).to_be_bytes());
        let mut reader = FragmentReader::new();
        reader.push(&feed);
        assert!(
            matches!(reader.next_unit(), Err(Fmp4Error::Unsupported(_))),
            "a box big enough to wrap the cursor was accepted"
        );
    }

    /// ffmpeg writes a `traf` only for tracks with buffered samples, and stops
    /// waiting for one that has gone `max_interleave_delta` without a packet —
    /// so a late-starting or gapped audio track produces video-only fragments
    /// followed by fragments that carry audio again. Taking the track list
    /// from the run's FIRST fragment published a silent segment whose audio
    /// bytes were copied into the mdat but described by no `traf` at all.
    #[test]
    fn a_track_that_joins_late_is_not_dropped_from_the_segment() {
        let feed = pipe("open-gop");
        let (init, frags, _) = read_all(&feed);
        let audio = init
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio)
            .expect("audio")
            .id;

        // Fragment 0 loses its audio traf, exactly as a muxer that had no
        // audio buffered would have written it.
        let mut first = frags[0].clone();
        first.tracks.retain(|t| t.track_id != audio);
        let run = vec![first, frags[1].clone(), frags[2].clone()];

        let merged = merge(&run, &init, 1).expect("merge");
        let mut feed = init.bytes.clone();
        feed.extend_from_slice(&merged.bytes);
        let (_, out, _) = read_all(&feed);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].track(audio).is_some(),
            "the segment published no audio at all"
        );
        let want: usize = run
            .iter()
            .filter_map(|f| f.track(audio))
            .map(|t| t.sample_count())
            .sum();
        assert_eq!(
            out[0].track(audio).map(|t| t.sample_count()),
            Some(want),
            "audio samples went missing between the fragments and the segment"
        );
    }

    /// The bottom rung of the sample-resolution ladder. ffmpeg always fills
    /// `tfhd`, so nothing in production reaches `trex` — which is precisely
    /// why it had to be wired up deliberately and tested here. Left
    /// unimplemented, a stream that leaned on it resolved every sample to zero
    /// duration: the segmenter would never reach its floor, never cut, and
    /// hold the whole film in memory.
    #[test]
    fn trex_defaults_are_the_bottom_rung_of_the_ladder() {
        let feed = pipe("open-gop");
        let (init, _, _) = read_all(&feed);
        let video_id = init.video().expect("video").id;

        // ffmpeg writes an all-zero `trex` and puts the real defaults in each
        // `tfhd`, so the fixture cannot exercise this rung as it stands. Patch
        // values into the moov's own `trex` — same length, same structure —
        // and the rung becomes reachable without inventing a whole moov.
        let mut bytes = init.bytes.clone();
        let mut patched = false;
        for i in 0..bytes.len().saturating_sub(28) {
            if &bytes[i..i + 4] != b"trex" {
                continue;
            }
            if be_u32(&bytes, i + 8) != video_id {
                continue;
            }
            bytes[i + 16..i + 20].copy_from_slice(&4_242u32.to_be_bytes()); // duration
            bytes[i + 20..i + 24].copy_from_slice(&1_234u32.to_be_bytes()); // size
            bytes[i + 24..i + 28].copy_from_slice(&0x0101_0000u32.to_be_bytes()); // flags
            patched = true;
            break;
        }
        assert!(patched, "no trex for the video track in the fixture's moov");
        let mut reader = FragmentReader::new();
        reader.push(&bytes);
        let Ok(Some(Unit::Init(init))) = reader.next_unit() else {
            panic!("the patched moov did not parse");
        };
        let video = init.video().expect("video").clone();
        assert_eq!(video.default_sample_duration, 4_242);
        assert_eq!(video.default_sample_size, 1_234);

        // A traf with a bare tfhd (no defaults) and a trun with no per-sample
        // values: everything has to come from trex.
        let mut trun = Vec::new();
        trun.extend_from_slice(&0x0000_0001u32.to_be_bytes()); // data-offset only
        trun.extend_from_slice(&2u32.to_be_bytes());
        trun.extend_from_slice(&0u32.to_be_bytes());
        let mut traf = Vec::new();
        traf.extend_from_slice(&16u32.to_be_bytes());
        traf.extend_from_slice(b"tfhd");
        traf.extend_from_slice(&0x0002_0000u32.to_be_bytes());
        traf.extend_from_slice(&video.id.to_be_bytes());
        traf.extend_from_slice(&((trun.len() + 8) as u32).to_be_bytes());
        traf.extend_from_slice(b"trun");
        traf.extend_from_slice(&trun);
        let mut moof = Vec::new();
        moof.extend_from_slice(&((traf.len() + 8 + 8) as u32).to_be_bytes());
        moof.extend_from_slice(b"moof");
        moof.extend_from_slice(&((traf.len() + 8) as u32).to_be_bytes());
        moof.extend_from_slice(b"traf");
        moof.extend_from_slice(&traf);
        moof.extend_from_slice(&8u32.to_be_bytes());
        moof.extend_from_slice(b"mdat");

        reader.push(&moof);
        let Ok(Some(Unit::Fragment(f))) = reader.next_unit() else {
            panic!("the hand-built fragment did not parse");
        };
        let tf = f.track(video.id).expect("video traf");
        for s in tf.samples() {
            assert_eq!(s.duration, video.default_sample_duration);
            assert_eq!(s.size, video.default_sample_size);
            assert_eq!(s.flags, video.default_sample_flags);
        }
    }

    /// Version-0 composition offsets are UNSIGNED. Writing a negative one into
    /// a version-0 `trun` clamped it to zero and shifted that frame's
    /// presentation time silently — the exact class of change framemd5 exists
    /// to forbid, arriving without tripping it. A run with a negative offset
    /// must come out as version 1.
    #[test]
    fn a_negative_composition_offset_forces_a_version_1_trun() {
        let feed = pipe("open-gop");
        let (init, frags, _) = read_all(&feed);
        let video = init.video().expect("video").id;
        let mut frag = frags[0].clone();
        let mut wanted = 0i64;
        if let Some(tf) = frag.tracks.iter_mut().find(|t| t.track_id == video) {
            if let Some(s) = tf.runs.first_mut().and_then(|r| r.samples.get_mut(1)) {
                s.cto = -600;
                wanted = -600;
            }
        }
        assert_eq!(wanted, -600, "the fixture fragment had no second sample");

        let merged = merge(&[frag], &init, 1).expect("merge");
        let mut feed = init.bytes.clone();
        feed.extend_from_slice(&merged.bytes);
        let (_, out, _) = read_all(&feed);
        let got = out[0]
            .track(video)
            .and_then(|t| t.runs.first())
            .and_then(|r| r.samples.get(1))
            .map(|s| s.cto);
        assert_eq!(got, Some(-600), "a negative offset was clamped away");
    }

    /// A source whose audio outlasts its video: ffmpeg keeps muxing audio-only
    /// fragments past the last video sample, and a run made only of those has
    /// zero video duration. `EXTINF` is summed from the VIDEO track by rule
    /// (§6.3 — audio drifts against the media by up to an AAC frame per
    /// fragment), so the tail would have published `#EXTINF:0.000000`: a
    /// segment the playlist claims takes no time at all.
    #[test]
    fn a_tail_with_no_video_left_still_reports_a_real_duration() {
        let feed = pipe("open-gop");
        let (init, frags, _) = read_all(&feed);
        let video = init.video().expect("video").id;
        let timescale = init.video().map(|v| v.timescale).unwrap_or(1);

        // A floor nothing can reach, so the whole run lands in one segment at
        // `finish()` — which is what the real tail does.
        let policy = CutPolicy::new(
            u32::MAX / timescale.max(1),
            u32::MAX / timescale.max(1),
            usize::MAX,
            u32::MAX,
            timescale,
        );
        let mut seg = Segmenter::new(init.clone(), policy);
        for f in frags.iter().take(3) {
            let mut stripped = f.clone();
            stripped.tracks.retain(|t| t.track_id != video);
            assert!(
                !stripped.tracks.is_empty(),
                "the fixture has no audio track"
            );
            assert!(seg.push(stripped).expect("push").is_none());
        }
        let published = seg.finish().expect("finish").expect("a final segment");
        assert_eq!(
            published.segment.video_ticks, 0,
            "video was supposed to be gone"
        );
        assert!(
            published.seconds > 0.1,
            "an audio-only tail published EXTINF {}",
            published.seconds
        );
    }

    /// Dexter: New Blood S01E01 ends its video at 57:11 but carries audio to
    /// 57:56. ffmpeg puts the last video frames and that 45-second audio tail
    /// in one final fragment. Measuring the fragment from its non-zero video
    /// duration published `EXTINF:0.333`, so AVPlayer ended and the client
    /// reopened at 57:11 forever instead of consuming the audio tail.
    #[test]
    fn a_mixed_final_fragment_uses_its_trailing_audio_duration() {
        let feed = pipe("open-gop");
        let (init, frags, _) = read_all(&feed);
        let video = init.video().expect("video");
        let audio = init
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .expect("audio");
        let mut tail = frags[0].clone();

        {
            let audio_fragment = tail
                .tracks
                .iter_mut()
                .find(|track| track.track_id == audio.id)
                .expect("audio fragment");
            let last_sample = audio_fragment
                .runs
                .iter_mut()
                .flat_map(|run| run.samples.iter_mut())
                .last()
                .expect("audio sample");
            last_sample.duration += 45 * audio.timescale;
        }

        let video_seconds = tail.video_duration(&init) as f64 / video.timescale.max(1) as f64;
        let audio_seconds = tail
            .track(audio.id)
            .map(|track| track.duration() as f64 / audio.timescale.max(1) as f64)
            .expect("audio duration");
        assert!(
            video_seconds > 0.0,
            "the regression needs a last video frame"
        );
        assert!(
            audio_seconds > video_seconds + 40.0,
            "the regression needs the material audio tail"
        );

        let policy = CutPolicy::new(
            u32::MAX / video.timescale.max(1),
            u32::MAX / video.timescale.max(1),
            usize::MAX,
            u32::MAX,
            video.timescale,
        );
        let mut seg = Segmenter::new(init, policy);
        assert!(seg.push(tail).expect("push").is_none());
        let published = seg.finish().expect("finish").expect("a final segment");

        assert!(published.segment.video_ticks > 0);
        assert_eq!(published.reason, CutReason::EndOfStream);
        assert!(
            (published.seconds - audio_seconds).abs() < 1e-6,
            "EXTINF {} did not cover the {:.6}s audio tail",
            published.seconds,
            audio_seconds
        );
    }

    /// The golden comparison SEGMENTER-PLAN §8.4 asked for, and the one that
    /// would have caught the bug that reached production: run BOTH muxers on
    /// the same source and require them to declare the same tracks.
    ///
    /// On a source with chapters — which every disc remux has and no `lavfi`
    /// fixture did — ffmpeg's plain mp4 muxer writes them as a QuickTime `text`
    /// track plus a `chpl` box, with `tref` links from the real tracks. Its HLS
    /// muxer does not. So the segmenter's stream carried a third track nothing
    /// asked for, in the init and in every fragment; Safari refused it outright
    /// and the player's error fallback re-encoded a 4K remux to 1080p, while
    /// Chrome ignored the extra track and played fine.
    ///
    /// The assertion is deliberately "the same as the muxer we replaced" rather
    /// than "video and audio only": the whole promise of this path is that the
    /// only thing a player sees differently is where the boundaries fall.
    #[test]
    fn the_pipe_declares_the_same_tracks_as_the_hls_muxer() {
        use crate::domain::MediaFile;
        use crate::testfixtures::source_with_chapters;
        use crate::transcode::{copy_pipe_args, hls_copy_args, Pacing};

        let src = source_with_chapters();
        // The chapters really are in the source, or this test proves nothing.
        let probe = run(Command::new(ffprobe())
            .args(["-v", "error", "-show_chapters", "-of", "csv=p=0"])
            .arg(&src));
        assert!(
            String::from_utf8_lossy(&probe).lines().count() >= 3,
            "the fixture lost its chapters"
        );

        let file = MediaFile {
            id: 1,
            item_id: 1,
            path: src.clone(),
            size: 1,
            mtime: 1,
            duration_ms: Some(12_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: Some("Main".into()),
            width: Some(640),
            height: Some(360),
            bit_depth: Some(8),
            hdr: None,
            hdr_format: None,
            bitrate: Some(1_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 1,
            audio_offset_ms: 0,
            probed: true,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let out_dir = dir.path().to_string_lossy().into_owned();

        // What the segmenter reads.
        let pipe_bytes = run(Command::new(ffmpeg()).args(copy_pipe_args(
            &file,
            0.0,
            None,
            true,
            Pacing::unpaced(),
            false,
        )));
        let mut reader = FragmentReader::new();
        reader.push(&pipe_bytes);
        let Ok(Some(Unit::Init(ours))) = reader.next_unit() else {
            panic!("the pipe produced no init");
        };

        // What ffmpeg's own HLS muxer writes for the same session.
        run(Command::new(ffmpeg()).args(hls_copy_args(
            &file,
            0.0,
            None,
            true,
            Pacing::unpaced(),
            false,
            &out_dir,
        )));
        let their_init = std::fs::read(dir.path().join("init.mp4")).expect("hlsenc init.mp4");
        let mut reader = FragmentReader::new();
        reader.push(&their_init);
        let Ok(Some(Unit::Init(theirs))) = reader.next_unit() else {
            panic!("the hls muxer produced no init");
        };

        // Track KINDS, in order — not timescales. Each muxer picks its own
        // (16000 here against hlsenc's 12288) and every duration in its own
        // stream is consistent with it, so a timescale difference is not a
        // divergence a player can see. A track that exists on one side and
        // not the other is.
        let shape = |i: &Init| -> Vec<TrackKind> { i.tracks.iter().map(|t| t.kind).collect() };
        assert_eq!(
            shape(&ours),
            shape(&theirs),
            "the pipe declares tracks the HLS muxer does not — a chapter or \
             data track reaching a player is what broke Safari"
        );
        assert!(
            ours.tracks.iter().all(|t| t.timescale > 0),
            "a track with no timescale"
        );
        assert!(
            !ours.tracks.iter().any(|t| t.kind == TrackKind::Other),
            "a non-media track in the init: {:?}",
            ours.tracks.iter().map(|t| t.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn merging_nothing_is_an_error_not_an_empty_segment() {
        let feed = pipe("open-gop");
        let (init, _, _) = read_all(&feed);
        assert!(merge(&[], &init, 1).is_err());
    }
}
