//! Turning several interrupted encodes into one finished asset.
//!
//! A producer is preempted whenever somebody presses play (see
//! [`crate::admission`]), so a 4K film is normally made in pieces: encode until
//! a viewer wants the hardware, stop, resume later from the last published
//! segment boundary. What the cache eventually serves has to be a single VOD
//! playlist that reads as though none of that happened.
//!
//! Everything here is pure — parts in, a playlist and a list of renames out —
//! because the failure modes are all arithmetic and every one of them is
//! invisible at runtime. A resume point half a second wrong is not an error; it
//! is half a second of the film missing, twenty minutes in, in a file nobody
//! watches until next week.
//!
//! The playlist is **written**, not grown. ffmpeg's `event` playlists are
//! appended to as segments appear, and it is tempting to publish one by adding
//! `#EXT-X-ENDLIST` to the end of the last part's file. That is a corruption
//! path with no upside: it inherits whatever partial line a killed ffmpeg left
//! behind, and it cannot express a timeline assembled from several runs at all.

/// One uninterrupted encode's output: what it produced, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    /// Segment filenames as this part's own playlist names them, in playlist
    /// order — `seg00000.ts` upward, because every part starts its numbering
    /// at zero.
    pub segments: Vec<String>,
    /// Each segment's `EXTINF`, in milliseconds, in the same order.
    ///
    /// From the playlist rather than computed from `SEGMENT_SECONDS`: a
    /// keyframe interval is a request, not a guarantee, and a segment that
    /// actually ran 2.08 seconds must say so or the timeline drifts by a
    /// little on every join.
    pub durations_ms: Vec<i64>,
}

impl Part {
    /// Read a part out of the playlist ffmpeg wrote for it.
    ///
    /// A trailing `#EXTINF` with no URI after it is a segment that was being
    /// written when the process was killed — which is the ordinary way a
    /// preempted part ends. It is dropped, because the bytes it describes are
    /// incomplete and the boundary this part resumes from must be one the
    /// viewer can actually play through.
    pub fn from_playlist(text: &str) -> Part {
        let mut segments = Vec::new();
        let mut durations_ms = Vec::new();
        let mut pending: Option<i64> = None;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(rest) = line.strip_prefix("#EXTINF:") {
                pending = rest
                    .split(',')
                    .next()
                    .and_then(|d| d.trim().parse::<f64>().ok())
                    .filter(|d| d.is_finite() && *d >= 0.0)
                    .map(|secs| (secs * 1000.0).round() as i64);
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let Some(ms) = pending.take() else {
                continue; // a URI with no EXTINF is not a segment we can time
            };
            segments.push(line.to_owned());
            durations_ms.push(ms);
        }
        Part {
            segments,
            durations_ms,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn duration_ms(&self) -> i64 {
        self.durations_ms.iter().sum()
    }
}

/// One segment of the finished asset: where its bytes are now, and what it is
/// called in the published playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// Path relative to the temp root: `part-000/seg00004.ts`.
    pub from: String,
    /// Filename in the published directory: `seg00021.ts`.
    pub to: String,
}

/// A finished asset, ready to publish.
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    /// Every segment, renumbered into one continuous run.
    pub placements: Vec<Placement>,
    /// The VOD playlist that names them.
    pub playlist: String,
    pub duration_ms: i64,
}

/// Where a resumed encode should start, given what has been published.
///
/// The sum of the published `EXTINF`s, not `segments * SEGMENT_SECONDS`. The
/// two differ by a few milliseconds per segment on any real source, and the
/// error accumulates in the direction that loses picture: resuming from a
/// point *after* the last published frame silently drops whatever is between.
pub fn resume_at_ms(parts: &[Part]) -> i64 {
    parts.iter().map(Part::duration_ms).sum()
}

/// Renumber every part's segments into one run and write the playlist that
/// names them.
///
/// The renumbering is why the parts cannot simply be concatenated: each one
/// starts at `seg00000.ts`, so three parts contain three files by that name.
pub fn assemble(parts: &[Part]) -> Assembled {
    let mut placements = Vec::new();
    let mut playlist = String::new();
    let mut duration_ms = 0i64;
    let mut longest_ms = 0i64;

    for (part_index, part) in parts.iter().enumerate() {
        for (segment, ms) in part.segments.iter().zip(&part.durations_ms) {
            let to = format!("seg{:05}.ts", placements.len());
            placements.push(Placement {
                from: format!("{}/{segment}", part_dir(part_index)),
                to: to.clone(),
            });
            playlist.push_str(&format!("#EXTINF:{:.6},\n{to}\n", *ms as f64 / 1000.0));
            duration_ms += ms;
            longest_ms = longest_ms.max(*ms);
        }
    }

    // TARGETDURATION is the longest segment rounded UP, and a player is
    // entitled to treat it as a promise: one that is too small makes an
    // otherwise valid playlist non-compliant, and some players refuse it
    // outright rather than adapting.
    let target = ((longest_ms + 999) / 1000).max(1);
    let header = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n"
    );
    Assembled {
        placements,
        playlist: format!("{header}{playlist}#EXT-X-ENDLIST\n"),
        duration_ms,
    }
}

/// The subdirectory one part is encoded into, under the temp root.
pub fn part_dir(index: usize) -> String {
    format!("part-{index:03}")
}

// ---- what is worth pre-transcoding ----------------------------------------

/// One thing to produce, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub file_id: i64,
    pub item_id: i64,
    pub title: String,
    /// For the log and the settings page: which rail this came off.
    pub reason: &'static str,
}

/// Where a candidate came from, in the order the producer should spend its
/// hardware on them.
///
/// Order is the whole design. Pre-transcoding is a bet, and the bet is only
/// worth making where the odds are good: an episode somebody is one click away
/// from playing is a far better bet than a film that merely arrived recently,
/// and the GPU can only be spent once.
pub const REASON_IN_PROGRESS: &str = "in progress";
pub const REASON_NEXT_UP: &str = "next up";
pub const REASON_RECENT: &str = "recently added";

/// Merge the rails into one work list, best bet first, with no repeats.
///
/// `rails` is in priority order and each candidate already knows which rail it
/// came off, so this is pure list work: take in order, skip anything already
/// taken, stop at the cap.
///
/// Deduplicated by file, not by item: the same episode reaching the list from
/// both Next Up and recently-added is one encode, and producing it twice would
/// be an hour of GPU spent to lose a race with itself. The first rail to name
/// a file also gives it its reason, which is why the better rail goes first.
pub fn rank(rails: &[Vec<Candidate>], limit: usize) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for rail in rails {
        for c in rail {
            if out.len() >= limit {
                return out;
            }
            if seen.insert(c.file_id) {
                out.push(c.clone());
            }
        }
    }
    out
}

/// Is this file worth pre-transcoding at all?
///
/// The honest test is "would the client that plays this have to transcode",
/// and the client is not here. What *is* knowable is whether the source is one
/// no client direct-plays: 4K, HDR, or 10-bit HEVC will be transcoded for
/// essentially any browser, so pre-transcoding one is a bet that pays for
/// almost every viewer. A 1080p H.264 MP4 is the opposite — most clients play
/// it untouched, and an entry produced for it is disk spent on a transcode
/// that will not be asked for.
///
/// This deliberately under-selects. The cost of missing a candidate is a
/// viewer waiting the normal two seconds for a live start; the cost of a wrong
/// one is gigabytes of cache and an hour of GPU spent on nothing.
pub fn worth_producing(file: &plurx_core::domain::MediaFile) -> bool {
    let codec = file.video_codec.as_deref().unwrap_or("");
    let heavy_codec = matches!(codec, "hevc" | "h265" | "av1" | "vvc");
    let uhd = file.height.unwrap_or(0) >= 2160;
    let hdr = file.hdr.is_some();
    // Unprobed files are excluded rather than guessed at: the recipe keys on
    // width, height and HDR, so producing from unknown values makes an entry
    // whose hash cannot match the one a real playback computes.
    file.probed && (uhd || hdr || heavy_codec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(durations: &[f64]) -> String {
        let mut s = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n");
        for (i, d) in durations.iter().enumerate() {
            s.push_str(&format!("#EXTINF:{d:.6},\nseg{i:05}.ts\n"));
        }
        s
    }

    /// The ordinary case, and the one thing it must get right: three parts
    /// contain three files called `seg00000.ts`, and the published asset has
    /// exactly one of each name in one continuous run.
    #[test]
    fn parts_are_renumbered_into_one_continuous_run() {
        let parts = [
            Part::from_playlist(&playlist(&[2.0, 2.0])),
            Part::from_playlist(&playlist(&[2.0])),
            Part::from_playlist(&playlist(&[2.0, 1.5])),
        ];
        let out = assemble(&parts);

        let names: Vec<&str> = out.placements.iter().map(|p| p.to.as_str()).collect();
        assert_eq!(
            names,
            [
                "seg00000.ts",
                "seg00001.ts",
                "seg00002.ts",
                "seg00003.ts",
                "seg00004.ts"
            ]
        );
        let sources: Vec<&str> = out.placements.iter().map(|p| p.from.as_str()).collect();
        assert_eq!(
            sources,
            [
                "part-000/seg00000.ts",
                "part-000/seg00001.ts",
                "part-001/seg00000.ts",
                "part-002/seg00000.ts",
                "part-002/seg00001.ts",
            ],
            "each part numbers from zero; only the published names are unique"
        );
        assert_eq!(out.duration_ms, 9_500);
        for name in names {
            assert!(out.playlist.contains(name), "{name} is not in the playlist");
        }
        assert!(out.playlist.ends_with("#EXT-X-ENDLIST\n"));
        assert!(out.playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    }

    /// A preempted part ends with an `#EXTINF` and no URI: ffmpeg was killed
    /// while writing that segment. Counting it would put the resume point past
    /// the last frame anyone can actually play, and the gap — a couple of
    /// seconds, somewhere in the middle of a film — would never be noticed
    /// until somebody watched it.
    #[test]
    fn a_segment_that_was_being_written_when_the_encoder_died_does_not_count() {
        let mut text = playlist(&[2.0, 2.0]);
        text.push_str("#EXTINF:2.000000,\n"); // killed here
        let part = Part::from_playlist(&text);
        assert_eq!(part.segments.len(), 2);
        assert_eq!(part.duration_ms(), 4_000);
        assert_eq!(resume_at_ms(&[part]), 4_000, "resume from a playable point");
    }

    /// The resume point is the sum of what was published, not a segment count
    /// times the nominal length. A keyframe interval is a request; real
    /// segments come out a few milliseconds long, and the error accumulates in
    /// the direction that loses picture.
    #[test]
    fn the_resume_point_is_measured_not_assumed() {
        let parts = [
            Part::from_playlist(&playlist(&[2.083, 2.083, 2.083])),
            Part::from_playlist(&playlist(&[1.958])),
        ];
        assert_eq!(resume_at_ms(&parts), 8_207);
        assert_ne!(
            resume_at_ms(&parts),
            4 * 2_000,
            "four segments is not eight seconds, and the difference is picture"
        );
    }

    /// `TARGETDURATION` is a promise about the longest segment, and players are
    /// entitled to enforce it. Rounding down, or defaulting it to the nominal
    /// segment length, produces a playlist that is simply invalid whenever a
    /// segment overruns — which they do, routinely.
    #[test]
    fn target_duration_covers_the_longest_segment() {
        let out = assemble(&[Part::from_playlist(&playlist(&[2.0, 3.4, 1.1]))]);
        assert!(
            out.playlist.contains("#EXT-X-TARGETDURATION:4"),
            "{}",
            out.playlist
        );
        // Never zero, however short the content — a target of 0 is invalid.
        let tiny = assemble(&[Part::from_playlist(&playlist(&[0.2]))]);
        assert!(tiny.playlist.contains("#EXT-X-TARGETDURATION:1"));
    }

    /// A run that produced nothing — preempted before its first segment
    /// completed — has to assemble to nothing rather than to a playlist with no
    /// segments, which would be published as a finished asset and served.
    #[test]
    fn a_part_that_produced_nothing_contributes_nothing() {
        let empty = Part::from_playlist("#EXTM3U\n#EXT-X-VERSION:3\n");
        assert!(empty.is_empty());
        assert_eq!(resume_at_ms(std::slice::from_ref(&empty)), 0);

        let out = assemble(&[
            Part::from_playlist(&playlist(&[2.0])),
            empty,
            Part::from_playlist(&playlist(&[2.0])),
        ]);
        assert_eq!(out.placements.len(), 2, "the empty part is simply skipped");
        assert_eq!(
            out.placements[1].from, "part-002/seg00000.ts",
            "and does not shift the numbering of the parts after it"
        );
    }

    /// Junk in a playlist must not become a segment. ffmpeg writes tags this
    /// does not know about, and a killed process can leave a truncated line;
    /// neither is a thing to publish.
    #[test]
    fn only_a_timed_uri_is_a_segment() {
        let part = Part::from_playlist(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             seg00000.ts\n\
             #EXTINF:notanumber,\n\
             seg00001.ts\n\
             #EXTINF:2.0,\n\
             seg00002.ts\n\
             #EXTINF:-1.0,\n\
             seg00003.ts\n",
        );
        assert_eq!(
            part.segments,
            ["seg00002.ts"],
            "a URI with no usable EXTINF is not a segment"
        );
        assert_eq!(part.duration_ms(), 2_000);
    }

    // ---- what is worth producing -----------------------------------------

    fn rail(reason: &'static str, items: &[(i64, &str)]) -> Vec<Candidate> {
        items
            .iter()
            .map(|(id, t)| Candidate {
                file_id: *id,
                item_id: *id + 1000,
                title: (*t).to_owned(),
                reason,
            })
            .collect()
    }

    /// Rail order is the design. The GPU can be spent once, and an episode
    /// somebody is one click from playing is a far better bet than a film that
    /// merely arrived recently.
    #[test]
    fn the_best_bets_are_produced_first() {
        let out = rank(
            &[
                rail(REASON_IN_PROGRESS, &[(1, "Heat")]),
                rail(REASON_NEXT_UP, &[(2, "Alien E2")]),
                rail(REASON_RECENT, &[(3, "New Film")]),
            ],
            10,
        );
        assert_eq!(
            out.iter().map(|c| c.reason).collect::<Vec<_>>(),
            [REASON_IN_PROGRESS, REASON_NEXT_UP, REASON_RECENT]
        );
        assert_eq!(out[0].title, "Heat");
    }

    /// The same file on two rails is one encode. Producing it twice would be
    /// an hour of GPU spent to lose a race with itself — and the second
    /// producer would find the first's claim and stand down anyway, having
    /// already started an encoder.
    #[test]
    fn a_file_on_two_rails_is_produced_once() {
        let out = rank(
            &[
                rail(REASON_NEXT_UP, &[(7, "Alien E2"), (8, "Heat")]),
                rail(REASON_RECENT, &[(8, "Heat"), (9, "New Film")]),
            ],
            10,
        );
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(
            out.iter().filter(|c| c.file_id == 8).count(),
            1,
            "the duplicate survived"
        );
        assert_eq!(
            out.iter().find(|c| c.file_id == 8).map(|c| c.reason),
            Some(REASON_NEXT_UP),
            "and kept the better of its two reasons"
        );
    }

    /// The cap is a cap on the *whole* list, not per rail — otherwise a server
    /// with eight users would attempt eight times what a server with one does,
    /// on the same single GPU.
    #[test]
    fn the_list_is_bounded_across_every_rail() {
        let items = [(1, "a"), (2, "b"), (3, "c"), (4, "d")];
        let out = rank(
            &[
                rail(REASON_IN_PROGRESS, &items),
                rail(REASON_NEXT_UP, &items),
            ],
            3,
        );
        assert_eq!(out.len(), 3);
    }

    fn media(
        height: i64,
        codec: &str,
        hdr: Option<&str>,
        probed: bool,
    ) -> plurx_core::domain::MediaFile {
        plurx_core::domain::MediaFile {
            id: 1,
            item_id: 1,
            path: std::path::PathBuf::from("/m/x.mkv"),
            size: 1,
            mtime: 1,
            duration_ms: Some(1000),
            container: Some("mkv".into()),
            video_codec: Some(codec.into()),
            video_profile: None,
            width: Some(height * 16 / 9),
            height: Some(height),
            bit_depth: Some(8),
            hdr: hdr.map(|h| h.to_owned()),
            hdr_format: None,
            bitrate: Some(1_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 0,
            audio_offset_ms: 0,
            probed,
        }
    }

    /// Pre-transcoding is a bet, and this is where the odds are read. It
    /// under-selects on purpose: a missed candidate costs a viewer the normal
    /// two-second live start, while a wrong one costs gigabytes of cache and an
    /// hour of GPU spent on a transcode nobody asks for.
    #[test]
    fn only_sources_that_nothing_direct_plays_are_worth_producing() {
        assert!(worth_producing(&media(2160, "h264", None, true)), "4K");
        assert!(
            worth_producing(&media(1080, "h264", Some("hdr10"), true)),
            "HDR"
        );
        assert!(worth_producing(&media(1080, "hevc", None, true)), "HEVC");
        assert!(worth_producing(&media(2160, "hevc", Some("hdr10"), true)));

        // The case that is not worth it: most clients play this untouched, so
        // an entry produced for it is disk spent on a transcode nobody wants.
        assert!(!worth_producing(&media(1080, "h264", None, true)));
        assert!(!worth_producing(&media(720, "h264", None, true)));

        // An unprobed file has no width, height or HDR to key a recipe on, so
        // anything produced from it hashes to a name no real playback computes.
        assert!(
            !worth_producing(&media(2160, "hevc", Some("hdr10"), false)),
            "producing from unknown metadata makes an entry that can never hit"
        );
    }
}
