//! How one session's video gets from an HDR source to SDR output.
//!
//! Today's path decodes on the GPU, downloads every frame to system memory,
//! tone-maps it through a float chain on the CPU, and uploads it back for the
//! encoder. PERF-PLAN §2.9 measured what that costs on identical work — same
//! resolution, same codec, same rung, same encoder, the only difference being
//! this chain:
//!
//! ```text
//!   4k-hevc-sdr   0.98x p10    4K decode, no tone-map
//!   4k-hdr10      0.71x p10    4K decode, CPU float tone-map
//! ```
//!
//! A quarter of the pipeline's throughput, and it is the quarter that takes a
//! session from above realtime to below it — which is the whole difference
//! between a stream that builds reserve and one that drains the viewer's.
//! A pipeline that keeps frames on the GPU deletes both copies and the float
//! maths with them.
//!
//! Which one a node uses is decided by *probe*, never by version sniffing or
//! by what the hardware claims. A graph that parses is not a graph that works:
//! it can move frames and still produce clipped gray, or lose the HDR metadata
//! at the decoder, or mistag the output, or be slower than the CPU chain it
//! replaced. `plurxd`'s pipeline probe checks each of those against real HDR10
//! and only then lets a node use a graph (PERF-PLAN §5).

use super::Encoder;

/// The video path for one session, from decoded frames to the encoder's input.
///
/// Ordered from most to least preferred; the probe walks candidates in this
/// order and takes the first that proves itself. [`Pipeline::Cpu`] is last and
/// unconditional — it needs no hardware, it is what every other variant falls
/// back to, and it is the reference the others are checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pipeline {
    /// Intel, frames never leave the GPU: `vpp_qsv` scales and tone-maps in
    /// one pass on the video-processing block.
    VppQsv,
    /// The VA-API equivalent — Intel, and AMD where the driver implements it.
    TonemapVaapi,
    /// Vulkan, vendor-neutral. The likely answer for the AMD boxes, whose VCN
    /// has no tone-map block of its own. Already reachable as
    /// `PLURX_TONEMAP=libplacebo`; the probe is what turns a blind preference
    /// into a checked capability.
    Libplacebo,
    /// OpenCL, jellyfin-ffmpeg's portable alternate. Needs an explicit
    /// download after the map — `tonemap_opencl` outputs OpenCL surfaces the
    /// H.264 encoders cannot take directly.
    TonemapOpencl,
    /// hwdownload → CPU float tone-map → the encoder's own hwupload. Always
    /// available, always correct, and slow in exactly the case that matters.
    Cpu,
}

/// Every pipeline the probe may consider, best first.
pub const CANDIDATES: &[Pipeline] = &[
    Pipeline::VppQsv,
    Pipeline::TonemapVaapi,
    Pipeline::Libplacebo,
    Pipeline::TonemapOpencl,
    Pipeline::Cpu,
];

impl Pipeline {
    /// Stable identifier for settings, logs, and the stats overlay.
    pub fn name(self) -> &'static str {
        match self {
            Pipeline::VppQsv => "vpp_qsv",
            Pipeline::TonemapVaapi => "tonemap_vaapi",
            Pipeline::Libplacebo => "libplacebo",
            Pipeline::TonemapOpencl => "tonemap_opencl",
            Pipeline::Cpu => "cpu",
        }
    }

    pub fn parse(s: &str) -> Option<Pipeline> {
        CANDIDATES.iter().copied().find(|p| p.name() == s)
    }

    /// Human label for the overlay and the admin log.
    pub fn label(self) -> &'static str {
        match self {
            Pipeline::VppQsv => "GPU tone-map (QSV)",
            Pipeline::TonemapVaapi => "GPU tone-map (VA-API)",
            Pipeline::Libplacebo => "GPU tone-map (Vulkan)",
            Pipeline::TonemapOpencl => "GPU tone-map (OpenCL)",
            Pipeline::Cpu => "CPU tone-map",
        }
    }

    /// True when frames stay on the GPU from decode to encode.
    pub fn on_gpu(self) -> bool {
        self != Pipeline::Cpu
    }

    /// Can this pipeline pair with `encoder`?
    ///
    /// The vendor graphs are tied to their own device — `vpp_qsv` needs the
    /// QSV device the QSV encoder initialises, and hands back QSV surfaces
    /// nothing else can take. The vendor-neutral ones are not, but they still
    /// need a hardware encoder on the other side: doing the tone-map on the GPU
    /// and then the H.264 encode on the CPU means downloading every frame
    /// anyway, which is the copy this exists to remove.
    pub fn pairs_with(self, encoder: Encoder) -> bool {
        match self {
            Pipeline::VppQsv => encoder == Encoder::Qsv,
            Pipeline::TonemapVaapi => encoder == Encoder::Vaapi,
            Pipeline::Libplacebo | Pipeline::TonemapOpencl => encoder != Encoder::Software,
            Pipeline::Cpu => true,
        }
    }

    /// Can this pipeline handle `hdr_format` (the source's `hdr` column)?
    ///
    /// HDR10 and HDR10+ are PQ and are what the vendor filters are built and
    /// tested for. HLG is a different transfer function and the vendor filters
    /// handle it inconsistently across drivers — a wrong answer there is a
    /// washed-out picture rather than a failure, which is the worst kind of
    /// bug because nobody files it. Dolby Vision is worse still: some profiles
    /// hardware-decode to garbage, which is why `PLURX_HWDECODE=off` exists.
    /// Both route to the CPU chain, and the graph-selection log line says so
    /// (PERF-PLAN §5 scope guards).
    pub fn handles(self, hdr_format: Option<&str>) -> bool {
        match (self, hdr_format) {
            (Pipeline::Cpu, _) => true,
            // Nothing to map. The graph is still allowed — its scaler is the
            // reason — and the tone-map step simply drops out.
            (_, None) => true,
            (_, Some(f)) => matches!(f, "hdr10" | "hdr10plus"),
        }
    }

    /// Decode-side flags. The GPU pipelines all need the decoder to hand back
    /// hardware surfaces — a pipeline that keeps frames on the GPU cannot
    /// start from frames that were never there.
    pub fn decode_args(self) -> Vec<String> {
        let a = |s: &str| s.to_owned();
        match self {
            Pipeline::VppQsv => vec![
                a("-hwaccel"),
                a("qsv"),
                a("-hwaccel_output_format"),
                a("qsv"),
            ],
            Pipeline::TonemapVaapi => vec![
                a("-hwaccel"),
                a("vaapi"),
                a("-hwaccel_output_format"),
                a("vaapi"),
            ],
            // Vulkan and OpenCL map from system-memory or VA-API frames; the
            // caller's existing decode choice stands, and the filter chain
            // uploads. Deriving decode flags here would fight `decode_setup`,
            // which knows things this type does not (source codec, bit depth).
            Pipeline::Libplacebo | Pipeline::TonemapOpencl | Pipeline::Cpu => Vec::new(),
        }
    }

    /// Device-init flags this pipeline needs before the input, beyond whatever
    /// the encoder already initialises.
    pub fn init_args(self) -> Vec<String> {
        let a = |s: &str| s.to_owned();
        match self {
            // libplacebo wants a Vulkan device; ffmpeg derives one from the
            // existing hardware context where it can, but naming it is what
            // makes the graph work on a box whose encoder is VA-API.
            Pipeline::Libplacebo => vec![
                a("-init_hw_device"),
                a("vulkan=vk"),
                a("-filter_hw_device"),
                a("vk"),
            ],
            Pipeline::TonemapOpencl => vec![
                a("-init_hw_device"),
                a("opencl=ocl"),
                a("-filter_hw_device"),
                a("ocl"),
            ],
            _ => Vec::new(),
        }
    }

    /// The scale + tone-map segment of the filter chain, for a source with
    /// `hdr_format` targeting `height`.
    ///
    /// Returns `None` for [`Pipeline::Cpu`], whose chain is built by the
    /// existing CPU path — this type describes the graphs that replace it, and
    /// having it also own the one it replaces would put two spellings of the
    /// CPU chain in the tree.
    ///
    /// `hdr_format` is taken rather than assumed because a GPU pipeline may be
    /// selected for a session with no HDR at all: the scaler is worth having
    /// on the GPU either way, and the tone-map step simply drops out.
    ///
    /// `width` pins the frame to an exact size instead of letting the scaler
    /// keep aspect on its own. A bitmap burn needs that: the subtitle plane is
    /// scaled by a *different* filter against `output_size`'s answer, and two
    /// scalers rounding independently is a composite that can miss by a pixel.
    /// `None` preserves the keep-aspect spellings, which also never upscale;
    /// an exact size is the caller promising it already clamped to the source.
    pub fn filters(
        self,
        width: Option<i64>,
        height: i64,
        hdr_format: Option<&str>,
    ) -> Option<String> {
        let hdr = hdr_format.is_some();
        let w = width.map_or_else(|| "-1".to_owned(), |w| w.to_string());
        Some(match self {
            Pipeline::Cpu => return None,
            // vpp_qsv does scale and tone-map in one pass. `w=-1` keeps the
            // aspect; the output is nv12 in GPU memory, which is what h264_qsv
            // wants, so no format conversion is needed on either side.
            Pipeline::VppQsv => {
                if hdr {
                    format!("vpp_qsv=w={w}:h={height}:tonemap=1:format=nv12")
                } else {
                    format!("vpp_qsv=w={w}:h={height}:format=nv12")
                }
            }
            // VA-API splits them: scale_vaapi resizes, tonemap_vaapi maps. Both
            // stay in VA-API surfaces, which h264_vaapi encodes directly.
            Pipeline::TonemapVaapi => {
                if hdr {
                    format!("scale_vaapi=w={w}:h={height}:format=p010,tonemap_vaapi=format=nv12:matrix=bt709:transfer=bt709:primaries=bt709")
                } else {
                    format!("scale_vaapi=w={w}:h={height}:format=nv12")
                }
            }
            // libplacebo scales and maps together and outputs to whatever the
            // next filter needs; `hwupload`/`hwdownload` around it are what let
            // it sit between decoders and encoders of different families.
            Pipeline::Libplacebo => {
                let tm = if hdr {
                    ":tonemapping=bt.2390:colorspace=bt709:color_primaries=bt709:color_trc=bt709"
                } else {
                    ""
                };
                format!(
                    "hwupload,libplacebo=w={w}:h={height}{tm}:format=nv12,hwdownload,format=nv12"
                )
            }
            // tonemap_opencl maps only, so the scale stays on the CPU side of
            // it. Its output is an OpenCL surface no H.264 encoder takes, hence
            // the explicit download.
            Pipeline::TonemapOpencl => {
                let scale = match width {
                    Some(w) => format!("scale={w}:{height}"),
                    None => format!("scale=-2:'min({height},ih)'"),
                };
                if hdr {
                    format!(
                        "{scale},format=p010,hwupload,\
                         tonemap_opencl=tonemap=hable:transfer=bt709:matrix=bt709:primaries=bt709:format=nv12,\
                         hwdownload,format=nv12"
                    )
                } else {
                    format!("{scale},format=nv12")
                }
            }
        })
    }

    /// The pipeline this session actually gets, given the one the node proved
    /// and what this particular session is doing.
    ///
    /// A probe answers "can this box run this graph"; it cannot answer "should
    /// this session". Two correctness constraints send a session back to the
    /// CPU chain no matter what the node proved, and both are quiet failures
    /// than loud ones — which is why they are decided here, once, instead of
    /// being left to the graph to discover:
    ///
    /// - **The source it can't map.** HLG and Dolby Vision (see
    ///   [`Pipeline::handles`] — but note `transcode::routing_hdr`: a DV
    ///   source whose base layer is HDR10-compatible routes as `hdr10`,
    ///   because it decodes to ordinary PQ surfaces and neither chain reads
    ///   the RPUs anyway).
    /// - **The encoder it can't feed.** A graph and an encoder from different
    ///   families produce surfaces the other cannot read (see
    ///   [`Pipeline::pairs_with`]).
    ///
    /// Text and bitmap subtitle burns both keep the GPU scale + tone-map. The
    /// mapped frame comes down exactly once for libass/overlay and returns to
    /// the hardware encoder; it is the CPU float tone-map, not that one copy,
    /// that decides whether a 4K burn can produce its first segment in time.
    ///
    /// And one more that is about worth rather than correctness: `heavy` (see
    /// `transcode::heavy_source`). The vendor graphs require hardware decode,
    /// and `decode_setup` deliberately keeps light sources on software decode
    /// — the GPU decode/filter handoff is a risk, and a 1080p H.264 scale is
    /// not a problem anyone has. A GPU path is for the sessions that are
    /// actually slow.
    pub fn for_session(
        proven: Pipeline,
        encoder: Encoder,
        hdr_format: Option<&str>,
        heavy: bool,
        burns_text_subtitles: bool,
    ) -> Pipeline {
        match Pipeline::declined(proven, encoder, hdr_format, heavy, burns_text_subtitles) {
            Some(_) => Pipeline::Cpu,
            None => proven,
        }
    }

    /// Why this session is not getting the graph the node proved, in words.
    ///
    /// `None` when it is getting it — or when there was nothing to decline,
    /// because the CPU chain is all this node has.
    ///
    /// [`Pipeline::for_session`] is defined in terms of this, so the decision
    /// and its explanation cannot drift apart. That matters more than it
    /// sounds. The doc above has said "the log says why" since M2 shipped, and
    /// the log did not: it printed `pipeline=cpu` on a 4K HDR session, on a box
    /// that had just probed a GPU graph at 4.9× — and left the reader to guess
    /// which of three conditions had fired. A fallback nobody can explain is
    /// indistinguishable from a bug, and this one is usually *correct*.
    pub fn declined(
        proven: Pipeline,
        encoder: Encoder,
        hdr_format: Option<&str>,
        heavy: bool,
        _burns_text_subtitles: bool,
    ) -> Option<&'static str> {
        if proven == Pipeline::Cpu {
            return None;
        }
        if !heavy {
            return Some("light source — a GPU graph is not worth the handoff");
        }
        if !proven.handles(hdr_format) {
            return Some(match hdr_format {
                Some("dolby_vision") => {
                    "Dolby Vision — the vendor tone-map cannot read its dynamic metadata, \
                     so the CPU chain is the correct answer rather than a fallback"
                }
                Some("hlg") => {
                    "HLG — the vendor tone-map handles it inconsistently across drivers, and \
                     a wrong answer there is a washed-out picture nobody reports"
                }
                _ => "the source's HDR format is not one the vendor tone-map is built for",
            });
        }
        if !proven.pairs_with(encoder) {
            return Some("the proven graph and this session's encoder are different families");
        }
        None
    }

    /// The next thing to try when this pipeline fails at runtime.
    ///
    /// One step, and it goes straight to the CPU chain rather than down the
    /// candidate list. A graph that passed its probe and then stopped
    /// progressing is evidence about *this driver right now* — GPU contention,
    /// a codec profile the probe's fixture didn't cover, a device that has
    /// wedged — and none of that makes the next GPU graph a better bet. The
    /// viewer is waiting; the second attempt should be the one that always
    /// works.
    pub fn fallback(self) -> Option<Pipeline> {
        (self != Pipeline::Cpu).then_some(Pipeline::Cpu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in CANDIDATES {
            assert!(seen.insert(p.name()), "duplicate name {}", p.name());
            assert_eq!(Pipeline::parse(p.name()), Some(*p));
        }
        assert_eq!(Pipeline::parse("gpu-magic"), None);
        // A stored preference from a future version must not panic anything.
        assert_eq!(Pipeline::parse(""), None);
    }

    /// The CPU chain is the floor: it pairs with everything, handles
    /// everything, and is what everything else falls back to. Any change that
    /// makes it conditional makes some session unplayable.
    #[test]
    fn the_cpu_pipeline_is_unconditional() {
        for encoder in [
            Encoder::Software,
            Encoder::Nvenc,
            Encoder::Qsv,
            Encoder::Vaapi,
            Encoder::VideoToolbox,
        ] {
            assert!(Pipeline::Cpu.pairs_with(encoder));
        }
        for hdr in [None, Some("hdr10"), Some("hlg"), Some("dolby_vision")] {
            assert!(Pipeline::Cpu.handles(hdr), "cpu must handle {hdr:?}");
        }
        assert_eq!(Pipeline::Cpu.fallback(), None, "nothing is below it");
        assert!(Pipeline::Cpu.filters(None, 1080, Some("hdr10")).is_none());
        assert!(!Pipeline::Cpu.on_gpu());
    }

    /// Vendor graphs belong to their vendor's device. Pairing `vpp_qsv` with a
    /// VA-API encoder builds a graph that hands QSV surfaces to something that
    /// cannot read them — a failure at play time, from a combination nothing
    /// else would have rejected.
    #[test]
    fn vendor_graphs_only_pair_with_their_own_encoder() {
        assert!(Pipeline::VppQsv.pairs_with(Encoder::Qsv));
        assert!(!Pipeline::VppQsv.pairs_with(Encoder::Vaapi));
        assert!(!Pipeline::VppQsv.pairs_with(Encoder::Software));
        assert!(Pipeline::TonemapVaapi.pairs_with(Encoder::Vaapi));
        assert!(!Pipeline::TonemapVaapi.pairs_with(Encoder::Qsv));

        // The neutral ones go with any hardware encoder — but not with
        // software, where the download they'd force is the copy the whole
        // exercise exists to remove.
        for p in [Pipeline::Libplacebo, Pipeline::TonemapOpencl] {
            assert!(p.pairs_with(Encoder::Vaapi), "{p:?} + vaapi");
            assert!(p.pairs_with(Encoder::Nvenc), "{p:?} + nvenc");
            assert!(!p.pairs_with(Encoder::Software), "{p:?} + software");
        }
    }

    /// HLG and Dolby Vision route to the CPU no matter what a GPU probe said.
    /// The failure mode for both is a washed-out or wrong picture rather than
    /// an error — which is the worst kind, because it plays and nobody reports
    /// it (PERF-PLAN §5 scope guards).
    #[test]
    fn only_pq_sources_reach_a_gpu_graph() {
        for p in CANDIDATES.iter().filter(|p| p.on_gpu()) {
            assert!(p.handles(Some("hdr10")), "{p:?} hdr10");
            assert!(p.handles(Some("hdr10plus")), "{p:?} hdr10+");
            assert!(!p.handles(Some("hlg")), "{p:?} must not take hlg");
            assert!(!p.handles(Some("dolby_vision")), "{p:?} must not take dv");
            // An SDR source has nothing to map, so nothing to get wrong — the
            // graph is chosen for its scaler there, not its tone-map.
            assert!(p.handles(None), "{p:?} sdr");
        }
    }

    /// A tone-map that never mentions bt709 is a tone-map that leaves the
    /// output tagged BT.2020 — which is how a correctly-mapped picture still
    /// renders wrong, and exactly what the probe's ffprobe assertion catches.
    #[test]
    fn every_gpu_graph_targets_bt709_for_hdr() {
        for p in CANDIDATES.iter().filter(|p| p.on_gpu()) {
            let hdr = p.filters(None, 1080, Some("hdr10")).expect("a graph");
            assert!(
                hdr.contains("bt709") || hdr.contains("tonemap=1"),
                "{p:?} does not retarget colour: {hdr}"
            );
            assert!(hdr.contains("1080"), "{p:?} ignores the rung: {hdr}");

            // With no HDR the tone-map step drops out — the scaler is still
            // worth having on the GPU, but mapping an SDR source to SDR is
            // work that changes nothing.
            let sdr = p.filters(None, 1080, None).expect("a graph");
            assert!(
                !sdr.contains("tonemap"),
                "{p:?} tone-maps an SDR source: {sdr}"
            );
            assert!(sdr.contains("1080"), "{p:?} ignores the rung: {sdr}");
        }
    }

    /// Filters that hand GPU surfaces to an encoder that cannot read them are
    /// the classic graph bug. The two neutral pipelines download explicitly;
    /// the vendor ones deliberately do not, because their own encoder wants
    /// the surface.
    #[test]
    fn neutral_graphs_download_and_vendor_graphs_do_not() {
        for p in [Pipeline::Libplacebo, Pipeline::TonemapOpencl] {
            let g = p.filters(None, 1080, Some("hdr10")).expect("a graph");
            assert!(
                g.contains("hwdownload"),
                "{p:?} leaves frames on the GPU: {g}"
            );
        }
        for p in [Pipeline::VppQsv, Pipeline::TonemapVaapi] {
            let g = p.filters(None, 1080, Some("hdr10")).expect("a graph");
            assert!(
                !g.contains("hwdownload"),
                "{p:?} round-trips needlessly: {g}"
            );
            assert!(!g.contains("hwupload"), "{p:?} round-trips needlessly: {g}");
        }
    }

    /// A proven graph is a claim about the box, not about the session. Each
    /// of these three would otherwise fail quietly — a washed picture, a
    /// broken graph at play time, or a round trip that costs more than it
    /// saves — so the routing is decided once, up front, and logged.
    #[test]
    fn a_session_can_be_sent_back_to_the_cpu_by_what_it_is_doing() {
        let proven = Pipeline::VppQsv;

        // The happy case: proven graph, its own encoder, a heavy PQ source,
        // no burn.
        assert_eq!(
            Pipeline::for_session(proven, Encoder::Qsv, Some("hdr10"), true, false),
            Pipeline::VppQsv
        );

        // HLG and DV go back regardless of what the probe proved. (A DV
        // source whose base layer is HDR10-compatible never presents as
        // "dolby_vision" here — `transcode::routing_hdr` routes it as hdr10
        // before this question is asked. What reaches this arm is the DV
        // that has no PQ base to fall back on, and the CPU chain is right.)
        for hdr in [Some("hlg"), Some("dolby_vision")] {
            assert_eq!(
                Pipeline::for_session(proven, Encoder::Qsv, hdr, true, false),
                Pipeline::Cpu,
                "{hdr:?} must not reach a GPU graph"
            );
        }

        // A graph that cannot feed the running encoder: the encoder is what
        // changed (a hardware fallback to software mid-life, say), and the
        // combination would build a graph nothing can read.
        assert_eq!(
            Pipeline::for_session(proven, Encoder::Software, Some("hdr10"), true, false),
            Pipeline::Cpu
        );

        // A text subtitle comes down after the GPU has done the expensive
        // scale + tone-map, renders in system memory, then returns to QSV.
        assert_eq!(
            Pipeline::for_session(proven, Encoder::Qsv, Some("hdr10"), true, true),
            Pipeline::VppQsv
        );

        // A heavy SDR source still gets the GPU scaler: no tone-map to do, but
        // a 4K resize is worth having on the video-processing block.
        assert_eq!(
            Pipeline::for_session(proven, Encoder::Qsv, None, true, false),
            Pipeline::VppQsv
        );

        // A light source does not. The GPU decode/filter handoff is a risk
        // taken for a reason, and a 1080p H.264 scale is not a problem.
        assert_eq!(
            Pipeline::for_session(proven, Encoder::Qsv, Some("hdr10"), false, false),
            Pipeline::Cpu,
            "a light source is not worth the handoff"
        );

        // And a node that proved nothing stays on the CPU chain, whatever it
        // is asked.
        assert_eq!(
            Pipeline::for_session(Pipeline::Cpu, Encoder::Qsv, Some("hdr10"), true, false),
            Pipeline::Cpu
        );
    }

    /// Every route back to the CPU chain can say which one it was, and the
    /// answer never disagrees with the decision.
    ///
    /// The pairing is the point. `pipeline=cpu` on a 4K HDR session, on a box
    /// whose probe just cleared a GPU graph at 4.9×, reads as the GPU path
    /// being broken — when the usual truth is Dolby Vision, where the CPU
    /// chain is *correct*. A silent right answer and a silent wrong one look
    /// identical, so neither may be silent.
    #[test]
    fn every_route_to_the_cpu_chain_says_which_one_it_was() {
        let proven = Pipeline::VppQsv;
        let cases: &[(&str, Option<&str>, bool, bool, Encoder)] = &[
            // (what we expect the reason to mention, hdr, heavy, burns, encoder)
            (
                "Dolby Vision",
                Some("dolby_vision"),
                true,
                false,
                Encoder::Qsv,
            ),
            ("HLG", Some("hlg"), true, false, Encoder::Qsv),
            ("light source", Some("hdr10"), false, false, Encoder::Qsv),
            ("families", Some("hdr10"), true, false, Encoder::Software),
        ];
        for (expect, hdr, heavy, burns, encoder) in cases {
            let why = Pipeline::declined(proven, *encoder, *hdr, *heavy, *burns);
            let why = why.unwrap_or_else(|| panic!("{expect}: no reason given"));
            assert!(
                why.contains(expect),
                "{expect}: reason does not say so — {why:?}"
            );
            assert_eq!(
                Pipeline::for_session(proven, *encoder, *hdr, *heavy, *burns),
                Pipeline::Cpu,
                "{expect}: a reason was given but the graph was used anyway"
            );
        }

        // A session that gets the proven graph has nothing to explain…
        assert_eq!(
            Pipeline::declined(proven, Encoder::Qsv, Some("hdr10"), true, false),
            None
        );
        // …and neither does a node that never had one to decline. Reporting
        // "light source" on a software-only box would be noise on every
        // session it ever runs.
        assert_eq!(
            Pipeline::declined(Pipeline::Cpu, Encoder::Software, None, false, false),
            None
        );
    }

    /// An explicit width pins every graph to the same frame the subtitle
    /// plane of a bitmap burn is scaled to. `None` keeps the keep-aspect
    /// spellings that also never upscale.
    #[test]
    fn an_exact_width_pins_the_frame_and_none_keeps_aspect() {
        let g = Pipeline::VppQsv
            .filters(Some(3840), 2160, Some("hdr10"))
            .expect("a graph");
        assert!(g.contains("vpp_qsv=w=3840:h=2160:tonemap=1"), "{g}");
        let g = Pipeline::VppQsv
            .filters(None, 1080, Some("hdr10"))
            .expect("a graph");
        assert!(g.contains("w=-1:h=1080"), "{g}");
        let g = Pipeline::TonemapOpencl
            .filters(Some(1920), 1080, Some("hdr10"))
            .expect("a graph");
        assert!(g.contains("scale=1920:1080,"), "{g}");
        let g = Pipeline::TonemapOpencl
            .filters(None, 1080, Some("hdr10"))
            .expect("a graph");
        assert!(g.contains("scale=-2:'min(1080,ih)'"), "{g}");
    }

    /// Failure goes to the floor, not to the next-best guess.
    #[test]
    fn a_failed_graph_falls_all_the_way_back() {
        for p in CANDIDATES.iter().filter(|p| p.on_gpu()) {
            assert_eq!(p.fallback(), Some(Pipeline::Cpu), "{p:?}");
        }
    }

    /// The vendor graphs need hardware surfaces to work on, so they must ask
    /// for them; the neutral ones must not, or they would override a decode
    /// choice made with knowledge they don't have.
    #[test]
    fn only_vendor_graphs_claim_the_decoder() {
        assert!(Pipeline::VppQsv.decode_args().contains(&"qsv".to_owned()));
        assert!(Pipeline::TonemapVaapi
            .decode_args()
            .contains(&"vaapi".to_owned()));
        for p in [Pipeline::Libplacebo, Pipeline::TonemapOpencl, Pipeline::Cpu] {
            assert!(
                p.decode_args().is_empty(),
                "{p:?} should not force a decoder"
            );
        }
        // …and the neutral ones need their own device instead.
        assert!(Pipeline::Libplacebo
            .init_args()
            .contains(&"vulkan=vk".to_owned()));
        assert!(Pipeline::TonemapOpencl
            .init_args()
            .contains(&"opencl=ocl".to_owned()));
        for p in [Pipeline::VppQsv, Pipeline::TonemapVaapi, Pipeline::Cpu] {
            assert!(p.init_args().is_empty(), "{p:?} needs no extra device");
        }
    }
}
