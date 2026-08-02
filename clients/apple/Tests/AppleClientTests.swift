import Darwin
import Foundation
import SwiftUI
import UIKit
import XCTest
@testable import plurx

private struct LayoutWidthPreferenceKey: PreferenceKey {
    static var defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

private struct LayoutFramePreferenceKey: PreferenceKey {
    static var defaultValue: CGRect = .null

    static func reduce(value: inout CGRect, nextValue: () -> CGRect) {
        let next = nextValue()
        value = value.isNull ? next : value.union(next)
    }
}

private extension View {
    func reportLayoutWidth() -> some View {
        background {
            GeometryReader { geometry in
                Color.clear.preference(
                    key: LayoutWidthPreferenceKey.self,
                    value: geometry.size.width
                )
            }
        }
    }

    func reportLayoutFrame() -> some View {
        background {
            GeometryReader { geometry in
                Color.clear.preference(
                    key: LayoutFramePreferenceKey.self,
                    value: geometry.frame(in: .global)
                )
            }
        }
    }
}

#if os(iOS)
private struct DetailNavigationTestHost<Content: View>: View {
    @State private var path = [1]
    @ViewBuilder let content: Content

    var body: some View {
        if #available(iOS 18.0, *) {
            tabs.tabViewStyle(.sidebarAdaptable)
        } else {
            tabs
        }
    }

    private var tabs: some View {
        TabView {
            NavigationStack(path: $path) {
                Color.clear
                    .navigationDestination(for: Int.self) { _ in
                        content
                    }
            }
            .tabItem { Label("Home", systemImage: "house") }
        }
    }
}
#endif

final class AppleClientTests: XCTestCase {
    override func tearDown() {
        Session.shared.origin = ""
        Session.shared.token = nil
        super.tearDown()
    }

    func testAppVersionLabelIncludesThePackageBuild() {
        XCTAssertEqual(
            AppBuildInfo.label(version: "0.2.0", build: "2"),
            "0.2.0 (2)"
        )
        XCTAssertEqual(AppBuildInfo.label(version: "0.2.0", build: nil), "0.2.0")
        XCTAssertEqual(AppBuildInfo.label(version: nil, build: nil), "Unknown")
    }

    func testDetailResumeUsesThePositionHandedBackByThePlayer() throws {
        var item = Item(id: 7, kind: "movie", title: "Feature")
        item.watch = Watch(positionMs: 12_000, durationMs: 7_200_000, watched: false)
        let original = ItemDetail(item: item)

        let updated = DetailView.detail(
            original,
            applyingPositionMs: 91_687,
            durationMs: 7_200_000,
            forItemId: 7
        )

        XCTAssertEqual(updated.item.watch?.positionMs, 91_687)
        XCTAssertEqual(updated.item.watch?.durationMs, 7_200_000)
        XCTAssertEqual(updated.item.watch?.watched, false)

        let unrelated = DetailView.detail(
            original,
            applyingPositionMs: 300_000,
            durationMs: 7_200_000,
            forItemId: 8
        )
        XCTAssertEqual(unrelated.item.watch?.positionMs, 12_000)
    }

    func testDetailProgressReflectsTheServerWatchedThreshold() throws {
        let original = ItemDetail(item: Item(id: 7, kind: "movie", title: "Feature"))

        let updated = DetailView.detail(
            original,
            applyingPositionMs: 95_000,
            durationMs: 100_000,
            forItemId: 7
        )

        XCTAssertEqual(updated.item.watch?.watched, true)
    }

    @MainActor
    func testTVSeriesPrimaryActionPrefersProgressAndSupportsSingleSeasonShapes() {
        var first = Item(id: 1, kind: "episode", title: "First")
        var progressing = Item(id: 2, kind: "episode", title: "In progress")
        var watched = Item(id: 3, kind: "episode", title: "Watched")
        first.watch = Watch(positionMs: 0, watched: false)
        progressing.watch = Watch(positionMs: 30_000, watched: false)
        watched.watch = Watch(positionMs: 0, watched: true)

        XCTAssertEqual(
            AppModel.orderedEpisodeCandidates([first, progressing, watched]).map(\.id),
            [2, 1, 3]
        )
        XCTAssertEqual(AppModel.resumableStartMs(positionMs: 30_000, durationMs: 100_000), 30_000)
        XCTAssertEqual(AppModel.resumableStartMs(positionMs: 96_000, durationMs: 100_000), 0)
    }

    @MainActor
    func testApplePlayerRetriesAnUnopenableOriginalOnlyOnce() {
        XCTAssertTrue(PlayerController.shouldRetryWithCompatibilityTranscode(
            canRetry: true,
            alreadyAttempted: false
        ))
        XCTAssertFalse(PlayerController.shouldRetryWithCompatibilityTranscode(
            canRetry: true,
            alreadyAttempted: true
        ))
        XCTAssertFalse(PlayerController.shouldRetryWithCompatibilityTranscode(
            canRetry: false,
            alreadyAttempted: false
        ))
    }

    @MainActor
    func testAutomaticSubtitlesFollowViewerLanguageNotContainerDefault() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "ita", title: "Forced",
                default: true, forced: true, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "ita", title: "Regular",
                default: false, forced: false, text: true
            ),
            // This is the affected Scary Movie shape: the mux retained the
            // English Forced title but omitted its forced disposition.
            SubtitleTrack(
                index: 2, codec: "subrip", language: "eng", title: "Forced",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 3, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 4, codec: "subrip", language: "eng", title: "SDH",
                default: false, forced: false, text: true
            ),
        ]

        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"),
            2
        )
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "en-US"),
            2
        )
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "spa")
        )
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "off")
        )
    }

    @MainActor
    func testAutomaticSubtitlesNeverStartABurnForABitmapOnlyLanguageMatch() {
        // A Blu-ray remux whose only English subtitle is a non-forced PGS.
        // Selecting it automatically would spawn a video encoder on every
        // single play — the exact bug this project exists to kill.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: true, forced: false, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "dvd_subtitle", language: "eng", title: "Commentary",
                default: false, forced: false, text: false
            ),
            SubtitleTrack(
                index: 2, codec: "subrip", language: "ita", title: "Italiano",
                default: false, forced: false, text: true
            ),
        ]

        XCTAssertNil(PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"))
        XCTAssertEqual(PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "ita"), 2)
    }

    @MainActor
    func testAutomaticSubtitlesNeverStartABurnForAStyledAssOnlyLanguageMatch() {
        // An anime MKV whose only English tracks are ASS: styled subtitles
        // stay burns, so automatic selection must decline them too.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "ass", language: "eng", title: "Full Subtitles",
                default: true, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "ssa", language: "eng", title: "Signs & Songs",
                default: false, forced: false, text: true
            ),
        ]

        XCTAssertNil(PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"))
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(0, in: tracks))
    }

    @MainActor
    func testAutomaticSubtitlesStillTakeAForcedBitmapTrackAtSourceHeight() {
        // Owner policy: automatic selection may never start a burn *except*
        // for a forced track, which burns at the source height so the picture
        // is not downgraded along with it.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Forced",
                default: false, forced: true, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: true, forced: false, text: false
            ),
        ]

        XCTAssertEqual(PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"), 0)
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(0, in: tracks))
        XCTAssertEqual(
            PlayerController.burnSessionHeight(
                burnSubtitle: 0, mode: "remux", selectedHeight: nil, sourceHeight: 2_160
            ),
            2_160,
            "an automatic forced burn keeps the source resolution"
        )
        XCTAssertEqual(
            PlayerController.burnSessionHeight(
                burnSubtitle: 0, mode: "remux", selectedHeight: 720, sourceHeight: 2_160
            ),
            720,
            "an explicit viewer rung still wins"
        )
        XCTAssertNil(PlayerController.burnSessionHeight(
            burnSubtitle: 0, mode: "transcode", selectedHeight: nil, sourceHeight: 2_160
        ))
        XCTAssertNil(PlayerController.burnSessionHeight(
            burnSubtitle: nil, mode: "remux", selectedHeight: nil, sourceHeight: 2_160
        ))
    }

    @MainActor
    func testAutomaticSubtitlesKeepUntaggedTracksEligibleLikeTheServerPolicy() {
        // The server's shared policy keeps untagged tracks eligible because a
        // missing tag is not contrary information (plurx-core tracks.rs).
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "ita", title: "Italiano",
                default: true, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: nil, title: "Forced",
                default: false, forced: false, text: true
            ),
        ]

        XCTAssertEqual(PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"), 1)

        var bitmapUntagged = tracks
        bitmapUntagged[1] = SubtitleTrack(
            index: 1, codec: "hdmv_pgs_subtitle", language: nil, title: "Forced",
            default: false, forced: true, text: false
        )
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(bitmapUntagged, preferredLanguage: "eng"),
            "an untagged burn-format track is not worth a burn"
        )
    }

    @MainActor
    func testNativeSubtitleOptionsMatchTheServerRenditionNameNotTheOrdinal() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "Forced",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 2, codec: "subrip", language: "eng", title: "SDH",
                default: false, forced: false, text: true
            ),
        ]
        // AVFoundation put a synthesized closed-caption-shaped entry ahead of
        // the authored renditions, so every ordinal is off by one.
        let options = [
            SubtitleRenditionOption(languageTag: "en", displayName: "English CC"),
            SubtitleRenditionOption(languageTag: "en", displayName: "English · Forced"),
            SubtitleRenditionOption(languageTag: "en", displayName: "English · SDH"),
        ]

        XCTAssertEqual(PlayerController.nativeSubtitleOrdinal(1, in: tracks), 0)
        XCTAssertEqual(
            PlayerController.nativeSubtitleOptionIndex(ordinal: 0, tracks: tracks, options: options),
            1,
            "the ordinal would have selected the phantom caption option"
        )
        XCTAssertEqual(
            PlayerController.nativeSubtitleOptionIndex(ordinal: 1, tracks: tracks, options: options),
            2
        )
        XCTAssertNil(
            PlayerController.nativeSubtitleOptionIndex(ordinal: 2, tracks: tracks, options: options)
        )
    }

    @MainActor
    func testSubtitleRenditionNamesReplicateTheServerRule() {
        func track(_ language: String?, _ title: String?) -> SubtitleTrack {
            SubtitleTrack(
                index: 0, codec: "subrip", language: language, title: title,
                default: false, forced: false, text: true
            )
        }

        XCTAssertEqual(
            PlayerController.subtitleRenditionName(track("eng", "Forced"), position: 2),
            "English · Forced"
        )
        XCTAssertEqual(
            PlayerController.subtitleRenditionName(track("ita", nil), position: 1),
            "Italian"
        )
        XCTAssertEqual(
            PlayerController.subtitleRenditionName(track(nil, "Commentary"), position: 0),
            "Commentary"
        )
        XCTAssertEqual(
            PlayerController.subtitleRenditionName(track(nil, "  "), position: 3),
            "Subtitle 4"
        )
        XCTAssertEqual(
            PlayerController.subtitleRenditionName(track("swe", nil), position: 0),
            "Swedish"
        )
        XCTAssertEqual(
            PlayerController.subtitleRenditionName(track("xyz", nil), position: 0),
            "xyz",
            "an unmapped tag passes through exactly as the server passes it"
        )
        XCTAssertEqual(PlayerController.subtitleLanguageTag("fra"), "fr")
        XCTAssertEqual(PlayerController.subtitleLanguageTag(nil), "und")
    }

    @MainActor
    func testSubtitleLanguageReplicasCoverTheWholeSharedAliasTable() {
        // The ten-language copy this replaced passed "dut"/"cze"/"gre"/"rum"
        // through untranslated, so name matching did nothing at all for them —
        // the population most exposed to a shifted ordinal.
        XCTAssertEqual(PlayerController.subtitleLanguageTag("dut"), "nl")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("nld"), "nl")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("cze"), "cs")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("gre"), "el")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("rum"), "ro")
        // Taken by length, not position: the Japanese group ends in "jp",
        // which is a country code and not a language subtag.
        XCTAssertEqual(PlayerController.subtitleLanguageTag("jpn"), "ja")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("jp"), "ja")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("EN"), "en")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("xyz"), "xyz")
        XCTAssertEqual(PlayerController.subtitleLanguageTag("  "), "und")

        XCTAssertEqual(PlayerController.subtitleLanguageName("dut"), "Dutch")
        XCTAssertEqual(PlayerController.subtitleLanguageName("cze"), "Czech")
        XCTAssertEqual(PlayerController.subtitleLanguageName("ell"), "Greek")
        XCTAssertEqual(PlayerController.subtitleLanguageName("ron"), "Romanian")
        XCTAssertEqual(PlayerController.subtitleLanguageName("nob"), "Norwegian")

        // The alias table also decides viewer-language matching, so a Dutch
        // preference has to reach a "dut"-tagged track.
        let dutch = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "dut", title: nil,
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(dutch, preferredLanguage: "nl"),
            0
        )
        XCTAssertEqual(PlayerController.languageSpellings("dut"), ["nl", "nld", "dut"])
        XCTAssertEqual(PlayerController.languageSpellings("eng"), ["en", "eng"])
        XCTAssertEqual(PlayerController.languageSpellings("xyz"), ["xyz"])
    }

    @MainActor
    func testRenditionNamesAreDeduplicatedTheWayTheMasterDeduplicatesThem() {
        // Two untitled English SRT tracks: RFC 8216 makes NAME unique, so the
        // server emits "English" and "English (2)". A replica that computes
        // only the base name resolves the second track onto the first — worse
        // than the ordinal guess the name matching replaced.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: nil,
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: nil,
                default: false, forced: false, text: true
            ),
        ]

        XCTAssertEqual(
            PlayerController.subtitleRenditionNames(tracks),
            ["English", "English (2)"]
        )

        // A phantom closed-caption option shifts the ordinals as well, so
        // neither positional nor base-name matching can rescue this.
        let options = [
            SubtitleRenditionOption(languageTag: "en", displayName: "English CC"),
            SubtitleRenditionOption(languageTag: "en", displayName: "English"),
            SubtitleRenditionOption(languageTag: "en", displayName: "English (2)"),
        ]
        XCTAssertEqual(
            PlayerController.nativeSubtitleOptionIndex(ordinal: 0, tracks: tracks, options: options),
            1
        )
        XCTAssertEqual(
            PlayerController.nativeSubtitleOptionIndex(ordinal: 1, tracks: tracks, options: options),
            2,
            "the second English track must not resolve onto the first"
        )

        // Bitmap tracks are not advertised, so they take no name and shift no
        // ordinal; positions still come from the whole subtitle list.
        let mixed = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: nil,
                default: false, forced: false, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: nil, title: nil,
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertEqual(PlayerController.subtitleRenditionNames(mixed), ["Subtitle 2"])
        XCTAssertEqual(
            PlayerController.quotedAttributeValue("He said \"go\"\nnow"),
            "He said 'go' now"
        )
    }

    @MainActor
    func testForcedTitlesMatchOnWordBoundariesAndHonorNegation() {
        // The forced arm is the only path by which automatic selection may
        // start a burn, so an over-eager title test burns ordinary tracks.
        XCTAssertTrue(PlayerController.titleMarksForced("Forced"))
        XCTAssertTrue(PlayerController.titleMarksForced("English Forced"))
        XCTAssertTrue(PlayerController.titleMarksForced("forced (signs)"))
        XCTAssertFalse(PlayerController.titleMarksForced("Non-Forced"))
        XCTAssertFalse(PlayerController.titleMarksForced("non forced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Not Forced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Unforced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Reinforced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Full"))

        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Non-Forced",
                default: false, forced: false, text: false
            ),
        ]
        XCTAssertFalse(PlayerController.isForcedSubtitle(tracks[0]))
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"),
            "a \"Non-Forced\" PGS track must not auto-burn on every play"
        )

        var flagged = tracks[0]
        flagged.forced = true
        XCTAssertTrue(
            PlayerController.isForcedSubtitle(flagged),
            "the container disposition still stands on its own"
        )
    }

    @MainActor
    func testAutomaticSubtitlesHonorTheServersAutoSubtitleMode() {
        // The server's default mode is Auto: audio already speaking the
        // preferred subtitle language leaves only the floor eligible
        // (crates/plurx-core/src/tracks.rs `select_tracks`).
        let full = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(
                full, preferredLanguage: "eng", audioLanguage: "eng"
            ),
            "English audio does not need a full English subtitle track"
        )
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(
                full, preferredLanguage: "eng", audioLanguage: "jpn"
            ),
            0,
            "foreign audio still gets full subtitles"
        )

        // The floor survives the mode: a forced overlay, or the pick the
        // server itself flagged, is offered under matching audio too.
        let floorTracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Forced",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(
                floorTracks, preferredLanguage: "eng", audioLanguage: "eng"
            ),
            0
        )
        var serverPicked = floorTracks
        serverPicked[0] = SubtitleTrack(
            index: 0, codec: "subrip", language: "eng", title: "SDH",
            default: true, forced: false, text: true
        )
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(
                serverPicked, preferredLanguage: "eng", audioLanguage: "eng"
            ),
            0,
            "`default` on a decision track is the server's own pick"
        )
    }

    @MainActor
    func testCompatibilityFallbackKeepsPositionPauseAndTheNativeSelection() {
        // The failed item's clock reads 0, so the retry resumes from the last
        // position the periodic observer saw, not from the dead item.
        XCTAssertEqual(
            PlayerController.compatibilityRetryPositionMs(lastObservedMs: 777_000),
            777_000
        )
        XCTAssertEqual(PlayerController.compatibilityRetryPositionMs(lastObservedMs: -1), 0)

        // A failed item has already dropped the rate to 0, so only the
        // viewer's own intent may decide whether the retry plays.
        XCTAssertTrue(PlayerController.reopenResumesPlayback(
            wantsPlayback: true, hasCurrentItem: true
        ))
        XCTAssertFalse(
            PlayerController.reopenResumesPlayback(wantsPlayback: false, hasCurrentItem: true),
            "a paused viewer stays paused across the retry"
        )
        XCTAssertTrue(
            PlayerController.reopenResumesPlayback(wantsPlayback: false, hasCurrentItem: false),
            "the first attach has nothing to preserve and always starts"
        )

        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false
            ),
        ]
        let native = PlayerController.sessionSubtitleFields(
            selected: 0, tracks: tracks, legacyBurn: false
        )
        XCTAssertEqual(native.native, 0, "the retry re-applies the native selection")
        XCTAssertNil(native.burn, "and never turns it into a burn")

        let bitmap = PlayerController.sessionSubtitleFields(
            selected: 1, tracks: tracks, legacyBurn: false
        )
        XCTAssertEqual(bitmap.burn, 1)
        XCTAssertNil(bitmap.native)

        let legacy = PlayerController.sessionSubtitleFields(
            selected: 0, tracks: tracks, legacyBurn: true
        )
        XCTAssertEqual(legacy.burn, 0, "only a legacy server burns a text track")
        XCTAssertNil(legacy.native)

        let off = PlayerController.sessionSubtitleFields(
            selected: nil, tracks: tracks, legacyBurn: true
        )
        XCTAssertNil(off.burn)
        XCTAssertNil(off.native)
    }

    @MainActor
    func testSelectSubtitleRoutingReopensOnceForABurnAndStaysInPlaceForNative() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Forced",
                default: false, forced: true, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 2, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false
            ),
        ]
        func route(_ index: Int?, activeBurn: Int? = nil) -> SubtitleSelectionRoute {
            PlayerController.subtitleSelectionRoute(
                for: index, tracks: tracks, activeBurn: activeBurn,
                isDirectPlayback: false
            )
        }

        XCTAssertEqual(route(0), .mediaSelection, "native → native stays in place")
        XCTAssertEqual(route(1), .mediaSelection)
        XCTAssertEqual(route(nil), .mediaSelection, "native → Off stays in place")
        XCTAssertEqual(route(2), .reopen, "entering a burn costs one reopen")
        XCTAssertEqual(
            route(1, activeBurn: 2),
            .reopen,
            "burn → native costs exactly one reopen, because the burn is in the frames"
        )
        XCTAssertEqual(route(nil, activeBurn: 2), .reopen)
    }

    @MainActor
    func testSubtitleSelectionChangedDuringOpenIsReconciledAfterwards() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Forced",
                default: false, forced: true, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 2, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false
            ),
        ]
        func reconcile(
            applied: Int?,
            current: Int?,
            activeBurn: Int? = nil,
            direct: Bool = false
        ) -> SubtitleSelectionRoute? {
            PlayerController.subtitleReconciliation(
                applied: applied,
                current: current,
                tracks: tracks,
                activeBurn: activeBurn,
                isDirectPlayback: direct
            )
        }

        XCTAssertNil(reconcile(applied: 0, current: 0), "the stream already matches the UI")
        XCTAssertEqual(reconcile(applied: 0, current: 1), .mediaSelection)
        XCTAssertEqual(reconcile(applied: 1, current: nil), .mediaSelection)
        XCTAssertEqual(reconcile(applied: nil, current: 2), .reopen)
        XCTAssertEqual(
            reconcile(applied: 2, current: nil, activeBurn: 2),
            .reopen,
            "leaving a burn always costs one reopen"
        )
        XCTAssertEqual(
            reconcile(applied: nil, current: 1, direct: true),
            .reopen,
            "P2-7: the first native selection creates the session"
        )
    }

    @MainActor
    func testDirectPlaySurvivesNativeTextTracksUntilTheFirstSelection() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
        ]

        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: nil, tracks: tracks, activeBurn: nil, isDirectPlayback: true
            ),
            .mediaSelection,
            "Off keeps true direct play"
        )
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 0, tracks: tracks, activeBurn: nil, isDirectPlayback: true
            ),
            .reopen
        )
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 0, tracks: tracks, activeBurn: nil, isDirectPlayback: false
            ),
            .mediaSelection
        )

        // The session that boundary creates carries the native fields, never
        // a burn — that is what makes the one reopen worth paying.
        let fields = PlayerController.sessionSubtitleFields(
            selected: 0, tracks: tracks, legacyBurn: false
        )
        XCTAssertEqual(fields.native, 0)
        XCTAssertNil(fields.burn)
    }

    @MainActor
    func testLegacyBurnFallbackIsGatedOnAServerWithoutNativeSubtitles() {
        // Every combination, because this gate is the guardrail against
        // sending `subtitle_burn` for a track a current server calls native.
        XCTAssertTrue(PlayerController.serverIsLegacy(
            servesNative: false, hasSubtitleOptions: false, isDirect: false
        ))
        XCTAssertFalse(
            PlayerController.serverIsLegacy(
                servesNative: true, hasSubtitleOptions: false, isDirect: false
            ),
            "a server that answered with a native master is never legacy, "
                + "however the selection failed"
        )
        XCTAssertFalse(PlayerController.serverIsLegacy(
            servesNative: true, hasSubtitleOptions: true, isDirect: false
        ))
        XCTAssertFalse(
            PlayerController.serverIsLegacy(
                servesNative: false, hasSubtitleOptions: true, isDirect: false
            ),
            "renditions exist, so the master is current and the option lookup lost"
        )
        XCTAssertFalse(
            PlayerController.serverIsLegacy(
                servesNative: false, hasSubtitleOptions: false, isDirect: true
            ),
            "direct play has no create response to have judged"
        )
        XCTAssertFalse(PlayerController.serverIsLegacy(
            servesNative: true, hasSubtitleOptions: false, isDirect: true
        ))
        XCTAssertFalse(PlayerController.serverIsLegacy(
            servesNative: false, hasSubtitleOptions: true, isDirect: true
        ))
        XCTAssertFalse(PlayerController.serverIsLegacy(
            servesNative: true, hasSubtitleOptions: true, isDirect: true
        ))

        XCTAssertTrue(PlayerController.playlistAdvertisesNativeSubtitles(
            "/api/v1/hls/2f9c/index.m3u8?native=1&subtitle=2"
        ))
        XCTAssertTrue(PlayerController.playlistAdvertisesNativeSubtitles(
            "http://media-box:32400/api/v1/hls/2f9c/index.m3u8?native=1"
        ))
        XCTAssertFalse(PlayerController.playlistAdvertisesNativeSubtitles(
            "/api/v1/hls/2f9c/index.m3u8"
        ))
        XCTAssertFalse(PlayerController.playlistAdvertisesNativeSubtitles(
            "/api/v1/hls/2f9c/index.m3u8?native=0"
        ))
    }

    @MainActor
    func testNativeSubtitleSwitchingUsesAVPlayerMediaSelectionWithoutAStreamReopen() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "Forced",
                default: true, forced: true, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false
            ),
            SubtitleTrack(
                index: 2, codec: "ass", language: "eng", title: "Styled",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 3, codec: "webvtt", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
        ]
        var selectedOrdinals: [Int?] = []

        XCTAssertTrue(PlayerController.applyNativeSubtitleSelection(
            3,
            tracks: tracks,
            select: { selectedOrdinals.append($0) }
        ))
        XCTAssertEqual(selectedOrdinals.count, 1)
        XCTAssertEqual(selectedOrdinals[0], 1, "bitmap/styled tracks are absent from HLS order")

        XCTAssertTrue(PlayerController.applyNativeSubtitleSelection(
            nil,
            tracks: tracks,
            select: { selectedOrdinals.append($0) }
        ))
        XCTAssertEqual(selectedOrdinals.count, 2)
        XCTAssertNil(selectedOrdinals[1], "Off deselects the AVPlayer option in place")

        XCTAssertFalse(PlayerController.applyNativeSubtitleSelection(
            1,
            tracks: tracks,
            select: { _ in XCTFail("PGS must use the burn/reopen fallback") }
        ))
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(1, in: tracks))
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(2, in: tracks))
        XCTAssertFalse(PlayerController.subtitleRequiresBurn(3, in: tracks))
    }

    func testOriginNormalizationAcceptsHostnamesAndRemovesTrailingSlashes() {
        XCTAssertEqual(AppModel.normalizeOrigin("  media-box:32400///  "), "http://media-box:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("media-box"), "http://media-box:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("192.168.1.20"), "http://192.168.1.20:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("http://192.168.1.20"), "http://192.168.1.20:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("https://media.example.test/"), "https://media.example.test")
        XCTAssertEqual(AppModel.normalizeOrigin("   "), "")
    }

    func testConnectionCodesAcceptServerAddressesAndRejectUnrelatedPayloads() {
        XCTAssertEqual(
            ConnectionCode.origin(from: "http://192.168.4.10:32400/"),
            "http://192.168.4.10:32400"
        )
        XCTAssertEqual(
            ConnectionCode.origin(
                from: "plurx://connect?origin=http%3A%2F%2Fmedia-box%3A32400"
            ),
            "http://media-box:32400"
        )
        XCTAssertNil(ConnectionCode.origin(from: "https://example.com/not-a-server-page"))
        XCTAssertNil(ConnectionCode.origin(from: "wifi password"))
    }

    func testSavedServerIdentityMatchesExactlyAndMigratesLegacyBonjourHosts() {
        let instanceId = "4f2cfb82-9162-4be0-a8bb-0123456789ab"
        XCTAssertTrue(AppModel.matchesSavedServer(
            candidateInstanceId: instanceId,
            expectedInstanceId: instanceId,
            savedOrigin: "http://old-address:32400"
        ))
        XCTAssertTrue(AppModel.matchesSavedServer(
            candidateInstanceId: instanceId,
            expectedInstanceId: nil,
            savedOrigin: "http://plurx-4f2cfb829162.local:32400"
        ))
        XCTAssertFalse(AppModel.matchesSavedServer(
            candidateInstanceId: "different-server",
            expectedInstanceId: instanceId,
            savedOrigin: "http://plurx-4f2cfb829162.local:32400"
        ))
    }

    func testSessionTokenMovesOutOfDefaultsAndSurvivesPreferenceReplacement() throws {
        final class MemoryTokenStore: TokenStoring {
            var value: String?
            func read() -> String? { value }
            func write(_ token: String) -> Bool { value = token; return true }
            func clear() { value = nil }
        }

        let suite = "tv.plurx.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let vault = MemoryTokenStore()

        defaults.set("legacy-token", forKey: "plurx.token")
        let migrated = SettingsStore(defaults: defaults, tokenVault: vault)
        XCTAssertEqual(migrated.token, "legacy-token")
        XCTAssertEqual(vault.value, "legacy-token")
        XCTAssertNil(defaults.string(forKey: "plurx.token"))

        defaults.removePersistentDomain(forName: suite)
        let restored = SettingsStore(defaults: defaults, tokenVault: vault)
        XCTAssertEqual(restored.token, "legacy-token")

        restored.clearToken()
        XCTAssertNil(restored.token)
    }

    func testBonjourOriginsHandleDnsNamesAndIpv6() {
        XCTAssertEqual(BonjourAddress.origin(host: "media-box.local.", port: 32400),
                       "http://media-box.local:32400")
        XCTAssertEqual(BonjourAddress.origin(host: "fe80::1", port: 32400),
                       "http://[fe80::1]:32400")
        XCTAssertEqual(BonjourAddress.origin(host: "fe80::1%en0", port: 32400),
                       "http://[fe80::1%25en0]:32400")
    }

    func testBonjourResolutionPrefersAFreshNumericAddress() {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        XCTAssertEqual(
            "192.168.4.42".withCString { inet_pton(AF_INET, $0, &address.sin_addr) },
            1
        )
        let data = Data(bytes: &address, count: MemoryLayout<sockaddr_in>.size)
        XCTAssertEqual(BonjourAddress.numericHost(from: [data]), "192.168.4.42")
    }

    func testRelativeMediaURLCarriesTokenAndPreservesExistingQuery() throws {
        Session.shared.origin = "http://media-box:32400"
        Session.shared.token = "secret token"

        let url = try XCTUnwrap(Session.shared.mediaURL("/api/v1/files/42/direct?download=1"))
        let components = try XCTUnwrap(URLComponents(url: url, resolvingAgainstBaseURL: false))
        let query = Dictionary(uniqueKeysWithValues: (components.queryItems ?? []).map { ($0.name, $0.value) })

        XCTAssertEqual(components.scheme, "http")
        XCTAssertEqual(components.host, "media-box")
        XCTAssertEqual(components.port, 32400)
        XCTAssertEqual(query["download"], "1")
        XCTAssertEqual(query["token"], "secret token")
    }

    func testAuthorizationHeaderUsesTheCurrentSessionToken() throws {
        Session.shared.token = "bearer-token"
        var request = URLRequest(url: try XCTUnwrap(URL(string: "https://media.example.test/api/v1/me")))

        Session.shared.authorize(&request)

        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer bearer-token")
    }

    func testAutoHlsRequestLeavesHeightUnsetAndCreatesAnIdempotencyKey() throws {
        let request = CreateSessionRequest(playbackId: "player-1", start: 12.5)
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(request)) as? [String: Any]
        )

        XCTAssertEqual(json["playback_id"] as? String, "player-1")
        XCTAssertEqual(json["start"] as? Double, 12.5)
        XCTAssertNotNil(json["request_id"] as? String)
        XCTAssertNil(json["height"])
        XCTAssertEqual(PlurxAPI.playbackPreparationTimeout, 180)
    }

    func testNativeSubtitleRequestDoesNotAskForBurnOrQualityChange() throws {
        let request = CreateSessionRequest(
            playbackId: "player-native-subs",
            start: 3_600,
            subtitleBurn: nil,
            nativeSubtitles: true,
            subtitle: 2,
            copy: true
        )
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(request)) as? [String: Any]
        )

        XCTAssertEqual(json["native_subtitles"] as? Bool, true)
        XCTAssertEqual(json["subtitle"] as? Int, 2)
        XCTAssertNil(json["subtitle_burn"])
        XCTAssertNil(json["height"], "subtitle selection must preserve Auto quality")
        XCTAssertEqual(json["copy"] as? Bool, true)
    }

    func testAppleCapsDescribeDolbyVisionProfilesWithoutDeprecatedHDRAPI() {
        let caps = Dictionary(uniqueKeysWithValues: Caps.query().compactMap { item in
            item.value.map { (item.name, $0) }
        })

        XCTAssertNotNil(caps["hdr"])
        XCTAssertNotNil(caps["dv"])
        XCTAssertNotNil(caps["dvprofile"])
        if caps["dv"] == "1" {
            XCTAssertEqual(caps["dvprofile"], "5,8")
        }
    }

    func testPictureInPictureCommandStartsStopsAndWaitsForAvailability() {
        XCTAssertEqual(
            PictureInPictureController.command(isActive: false, isPossible: true),
            .start
        )
        XCTAssertEqual(
            PictureInPictureController.command(isActive: true, isPossible: false),
            .stop
        )
        XCTAssertEqual(
            PictureInPictureController.command(isActive: false, isPossible: false),
            .unavailable
        )
    }

    func testAppDeclaresBackgroundAudioForPictureInPicture() {
        let modes = Bundle.main.object(forInfoDictionaryKey: "UIBackgroundModes") as? [String] ?? []
        XCTAssertTrue(modes.contains("audio"))
    }

    func testCopySessionCanPreserveNegotiatedDolbyVision() throws {
        let request = CreateSessionRequest(
            playbackId: "player-dv",
            copy: true,
            aac: false,
            preserveDolbyVision: true
        )
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(request)) as? [String: Any]
        )

        XCTAssertEqual(json["preserve_dolby_vision"] as? Bool, true)
    }

    func testPlayContextKeepsTheServerDurationForProgressReporting() {
        let context = PlayContext(itemId: 7, fileId: 11, startMs: 3_000,
                                  durationMs: 7_200_000, title: "Feature",
                                  overview: "A precise description.")

        XCTAssertEqual(context.durationMs, 7_200_000)
        XCTAssertEqual(context.overview, "A precise description.")
    }

    func testPlayerFactsSurfaceUsefulSourceInformation() {
        let source = SourceSummary(
            container: nil,
            videoCodec: "hevc",
            videoProfile: nil,
            width: nil,
            height: 2160,
            bitDepth: nil,
            hdr: nil,
            hdrFormat: "Dolby Vision · Profile 8",
            bitrate: nil,
            durationMs: nil
        )
        let audio = AudioTrack(
            index: 0,
            codec: "eac3",
            channels: 6,
            language: "eng",
            title: nil,
            default: true
        )

        XCTAssertEqual(
            PlayerView.playbackFacts(source: source, audio: audio),
            ["4K", "Dolby Vision", "Dolby Digital Plus 5.1 channels"]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.mark),
            [nil, "DV", "DD+ 5.1"]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.symbol),
            ["4k.tv.fill", "sparkles", "waveform"]
        )
        XCTAssertLessThanOrEqual(PlayerMetadataBadgeMetrics.rowSpacing, 6)
        XCTAssertLessThanOrEqual(PlayerMetadataBadgeMetrics.horizontalPadding, 6)
        XCTAssertLessThanOrEqual(PlayerMetadataBadgeMetrics.verticalPadding, 2)

        // Resolution tiers use both edges, so orientation ordering cannot turn
        // 1080p into 1920p and cropped scope masters retain their 4K tier.
        var orientationOrderedSource = source
        orientationOrderedSource.width = 1080
        orientationOrderedSource.height = 1920
        orientationOrderedSource.hdrFormat = nil
        XCTAssertEqual(
            PlayerView.playbackFacts(source: orientationOrderedSource, audio: nil),
            ["1080p"]
        )
        var scope4KSource = source
        scope4KSource.width = 3840
        scope4KSource.height = 1608
        scope4KSource.hdrFormat = nil
        XCTAssertEqual(
            PlayerView.playbackFacts(source: scope4KSource, audio: nil),
            ["4K"]
        )
    }

    func testPlayerOverlayAutoHidesWheneverItIsIdle() {
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: true,
            changingStream: false
        ))
        XCTAssertTrue(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: false,
            changingStream: false
        ))
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: false,
            scrubbing: false,
            changingStream: false
        ))
    }

    func testNowPlayingSummaryUsesLoadedOverviewAndHasAFallback() {
        XCTAssertEqual(
            PlayerView.nowPlayingSummary("  A family crosses the stars.\n"),
            "A family crosses the stars."
        )
        XCTAssertEqual(PlayerView.nowPlayingSummary("  "), "No description available.")
        XCTAssertEqual(PlayerView.nowPlayingSummary(nil), "No description available.")
    }

    func testPlayerOffersDownCueUntilNowPlayingInfoIsVisible() {
        XCTAssertEqual(PlayerView.nowPlayingInfoCueLabel(showingInfo: false), "INFO")
        XCTAssertNil(PlayerView.nowPlayingInfoCueLabel(showingInfo: true))
    }

    func testDetailViewportAndBodyNeverOutgrowTheirAvailableWidth() {
        for availableWidth: CGFloat in [320, 390, 430, 744, 1_366] {
            let controller = UIHostingController(rootView: DetailViewportFrame {
                DetailBodyFrame {
                    Text(String(repeating: "A wide detail overview. ", count: 20))
                        .fixedSize(horizontal: false, vertical: true)
                }
            })
            let measured = controller.sizeThatFits(
                in: CGSize(width: availableWidth, height: 10_000)
            )

            XCTAssertLessThanOrEqual(measured.width, availableWidth + 0.5)
        }
    }

    #if os(iOS)
    func testDetailBodyKeepsScrollableRowsAndActionsInsidePhoneInsets() throws {
        for viewportWidth: CGFloat in [393, 440] {
            let expectedBodyWidth = viewportWidth - (2 * screenHPad)
            var laidOutWidth: CGFloat = 0
            var laidOutFrame: CGRect = .null
            let controller = UIHostingController(rootView:
                DetailNavigationTestHost {
                    DetailViewportFrame {
                        DetailBodyFrame {
                            VStack(alignment: .leading, spacing: 12) {
                                ItemMetadataBadgeRow(badges: [
                                    ItemMetadataBadge(
                                        kind: .episode,
                                        symbol: "rectangle.stack.fill",
                                        mark: "S4 E3",
                                        accessibilityLabel: "Season 4, Episode 3"
                                    ),
                                    ItemMetadataBadge(
                                        kind: .runtime,
                                        symbol: "clock.fill",
                                        mark: "42 min",
                                        accessibilityLabel: "42 min"
                                    ),
                                    ItemMetadataBadge(
                                        kind: .resolution,
                                        symbol: "tv.fill",
                                        mark: "1080P",
                                        accessibilityLabel: "1080P"
                                    ),
                                    ItemMetadataBadge(
                                        kind: .video,
                                        symbol: "film.fill",
                                        mark: "H.264",
                                        accessibilityLabel: "H.264"
                                    ),
                                ])

                                Text("'Til Death Do You Part")
                                    .font(.largeTitle.bold())
                                    .lineLimit(nil)
                                    .multilineTextAlignment(.leading)
                                    .frame(maxWidth: .infinity, alignment: .leading)

                                PrimaryButton(title: "Resume · 0:44", action: {})
                            }
                            .reportLayoutWidth()
                            .reportLayoutFrame()
                        }
                    }
                }
                .dynamicTypeSize(.xxLarge)
                .onPreferenceChange(LayoutWidthPreferenceKey.self) {
                    laidOutWidth = $0
                }
                .onPreferenceChange(LayoutFramePreferenceKey.self) {
                    laidOutFrame = $0
                }
            )

            controller.view.frame = CGRect(
                origin: .zero,
                size: CGSize(width: viewportWidth, height: 800)
            )
            let window = UIWindow(frame: controller.view.frame)
            window.rootViewController = controller
            window.makeKeyAndVisible()
            controller.view.setNeedsLayout()
            controller.view.layoutIfNeeded()
            RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.2))

            XCTAssertFalse(laidOutFrame.isNull)
            XCTAssertLessThanOrEqual(laidOutWidth, expectedBodyWidth + 0.5)
            XCTAssertGreaterThanOrEqual(laidOutFrame.minX, screenHPad - 1)
            XCTAssertLessThanOrEqual(
                laidOutFrame.maxX,
                viewportWidth - screenHPad + 1
            )
            window.isHidden = true
        }
    }
    #endif

    func testEpisodeBreadcrumbLinksToTheShowAndSeasonInOrder() {
        let show = Item(id: 10, kind: "show", title: "Shameless")
        let season = Item(id: 20, kind: "season", title: "Season 1")

        XCTAssertEqual(
            [show, season].map(DetailBreadcrumb.destination(for:)),
            [.item(10), .item(20)]
        )
        XCTAssertGreaterThanOrEqual(DetailBreadcrumbMetrics.itemSpacing, 6)
        XCTAssertLessThanOrEqual(DetailBreadcrumbMetrics.verticalPadding, 4)
        XCTAssertLessThanOrEqual(DetailBreadcrumbMetrics.focusStrokeWidth, 1)
    }

    #if os(tvOS)
    func testTVHomeStartsWithMediaRailsInsteadOfAFeaturedBillboard() {
        XCTAssertFalse(HomeLayoutPolicy.usesFeaturedHero)
        XCTAssertEqual(HomeLayoutPolicy.continueWatchingCopyStyle, .accentPanel)
        XCTAssertEqual(
            HomeLayoutPolicy.topLevelTabs,
            ["Home", "Libraries", "Search", "Settings"]
        )
        XCTAssertFalse(HomeLayoutPolicy.showsLibraryShelvesOnHome)
        XCTAssertLessThanOrEqual(LandscapeAccentPanelMetrics.fillOpacity, 0.06)
        XCTAssertLessThanOrEqual(LandscapeAccentPanelMetrics.strokeOpacity, 0.18)
        XCTAssertLessThanOrEqual(LandscapeAccentPanelMetrics.strokeWidth, 0.5)
    }

    func testTVSeriesDetailKeepsCorrectArtworkRatioAndVisibleChildShelf() {
        XCTAssertEqual(
            TVSeriesDetailMetrics.posterHeight / TVSeriesDetailMetrics.posterWidth,
            1.5,
            accuracy: 0.001
        )
        XCTAssertLessThanOrEqual(
            TVSeriesDetailMetrics.headerHeight + 320,
            900,
            "the first row must begin inside the usable area below the tvOS tab bar"
        )
        XCTAssertEqual(DetailView.tvSeriesChildStyle(for: "show"), .poster)
        XCTAssertEqual(DetailView.tvSeriesChildStyle(for: "season"), .episode)
    }

    func testTVPlayableDetailUsesOneCinematicViewportAndUsefulMetadata() throws {
        XCTAssertLessThanOrEqual(
            TVPlayableDetailMetrics.heroHeight,
            720,
            "title, synopsis, and actions must remain visible below the tvOS tab bar"
        )
        XCTAssertGreaterThanOrEqual(TVPlayableDetailMetrics.copyWidth, 800)
        XCTAssertLessThanOrEqual(
            TVPlayableDetailMetrics.bottomInset,
            32,
            "the information group belongs against the lower television edge"
        )

        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let item = try decoder.decode(
            Item.self,
            from: Data(#"{"id":7,"kind":"episode","title":"Fray","year":2026,"season_number":4,"episode_number":2,"runtime_ms":3245000}"#.utf8)
        )
        let file = try decoder.decode(
            MediaFile.self,
            from: Data(#"{"id":11,"duration_ms":3245000,"container":"mkv","video_codec":"hevc","height":2160}"#.utf8)
        )

        XCTAssertEqual(
            DetailView.tvPlayableMetadata(item, file: file, durationMs: file.durationMs),
            "Season 4, Episode 2   ·   54 min   ·   4K   ·   HEVC"
        )
        XCTAssertEqual(
            DetailView.tvPlayableMetadataParts(item, file: file, durationMs: file.durationMs),
            ["Season 4, Episode 2", "54 min", "4K", "HEVC"]
        )
        let badges = DetailView.itemMetadataBadges(
            item,
            file: file,
            durationMs: file.durationMs,
            includeSeries: false
        )
        XCTAssertEqual(
            badges.map(\.symbol),
            ["rectangle.stack.fill", "clock.fill", "4k.tv.fill", "film.fill"]
        )
        XCTAssertEqual(badges.map(\.accessibilityLabel), [
            "Season 4, Episode 2", "54 min", "4K", "HEVC",
        ])
    }

    func testTVActionButtonsRemainReadableWithAndWithoutFocus() {
        for prominent in [false, true] {
            for focused in [false, true] {
                let foreground = TVReadableButtonStyle.foregroundColor(
                    prominent: prominent,
                    focused: focused
                )
                let background = TVReadableButtonStyle.backgroundColor(
                    prominent: prominent,
                    focused: focused
                )

                XCTAssertGreaterThanOrEqual(
                    contrastRatio(foreground, background),
                    4.5,
                    "prominent=\(prominent), focused=\(focused)"
                )
            }
        }
    }

    func testTVShelfActionsRemainReadableWithAndWithoutFocus() {
        for focused in [false, true] {
            XCTAssertGreaterThanOrEqual(
                contrastRatio(
                    TVShelfActionButtonStyle.foregroundColor(focused: focused),
                    TVShelfActionButtonStyle.backgroundColor(focused: focused)
                ),
                4.5,
                "focused=\(focused)"
            )
        }
    }

    func testTVMediaCardFocusSurroundIsThinAndFadesAtBothEdges() {
        XCTAssertLessThanOrEqual(
            TVMediaCardButtonStyle.outerStrokeWidth,
            6,
            "the complete focus surround should remain a thin ring"
        )
        XCTAssertGreaterThan(
            TVMediaCardButtonStyle.outerStrokeWidth,
            TVMediaCardButtonStyle.fadeStrokeWidth
        )
        XCTAssertGreaterThan(
            TVMediaCardButtonStyle.fadeStrokeWidth,
            TVMediaCardButtonStyle.accentStrokeWidth
        )
        XCTAssertGreaterThan(
            TVMediaCardButtonStyle.contentClearance,
            TVMediaCardButtonStyle.outerStrokeWidth / 2,
            "the curved ring must stay outside card text"
        )
        XCTAssertEqual(
            TVMediaCardButtonStyle.outerStrokeWidth - TVMediaCardButtonStyle.fadeStrokeWidth,
            TVMediaCardButtonStyle.fadeStrokeWidth - TVMediaCardButtonStyle.accentStrokeWidth,
            "the red core should taper evenly toward both black edges"
        )
    }

    func testTVPlayerChromeStaysCompactWithAHairlineProgressFocusRing() {
        XCTAssertLessThanOrEqual(TVPlayerControlButtonStyle.width, 54)
        XCTAssertLessThanOrEqual(TVPlayerControlButtonStyle.height, 46)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.headerHorizontalInset, 8)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.headerVerticalInset, 5)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.timeHorizontalInset, 6)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.timeVerticalInset, 3)
        XCTAssertGreaterThanOrEqual(TVPlayerChromeMetrics.infoBodyFontSize, 18)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.infoBodyFontSize, 20)
        XCTAssertGreaterThanOrEqual(TVPlayerChromeMetrics.infoLineLimit, 6)
        XCTAssertLessThanOrEqual(TVPlayerProgressFocusRing.outerStrokeWidth, 1.5)
        XCTAssertGreaterThan(
            TVPlayerProgressFocusRing.outerStrokeWidth,
            TVPlayerProgressFocusRing.fadeStrokeWidth
        )
        XCTAssertGreaterThan(
            TVPlayerProgressFocusRing.fadeStrokeWidth,
            TVPlayerProgressFocusRing.accentStrokeWidth
        )
    }

    func testShelfMetadataUsesMediaFactsInsteadOfLibraryCategory() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let episode = try decoder.decode(
            Item.self,
            from: Data(#"{"id":1,"kind":"episode","title":"Fray","season_number":4,"episode_number":2,"year":2026,"watch":{"position_ms":600000,"duration_ms":3240000}}"#.utf8)
        )
        let movie = try decoder.decode(
            Item.self,
            from: Data(#"{"id":2,"kind":"movie","title":"TRON: Ares","year":2025,"resolution":2160,"watch":{"position_ms":300000,"duration_ms":7200000}}"#.utf8)
        )

        XCTAssertEqual(cardShelfMetadata(episode), "S4 E2 · 44m left")
        XCTAssertEqual(cardShelfMetadata(movie), "2025 · 115m left")
        XCTAssertEqual(resolutionLabel(movie.resolution), "4K")
        XCTAssertFalse(cardShelfMetadata(movie).contains("4K"))
        XCTAssertFalse(cardShelfMetadata(episode).contains("TV"))
        XCTAssertFalse(cardShelfMetadata(movie).contains("Movies"))
    }

    func testMixedLandscapeShelfReservesEpisodeSubtitleLine() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let movie = try decoder.decode(
            Item.self,
            from: Data(#"{"id":1,"kind":"movie","title":"TRON: Ares","year":2025}"#.utf8)
        )
        let episode = try decoder.decode(
            Item.self,
            from: Data(#"{"id":2,"kind":"episode","title":"Fray","show_title":"FROM","season_number":4,"episode_number":2}"#.utf8)
        )

        XCTAssertFalse(landscapeShelfNeedsEpisodeSubtitleLine([movie]))
        XCTAssertTrue(landscapeShelfNeedsEpisodeSubtitleLine([movie, episode]))
    }

    func testContinueWatchingUsesTwoRowsAndRightAlignedTimeCopy() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let episode = try decoder.decode(
            Item.self,
            from: Data(#"{"id":1,"kind":"episode","title":"Fray","show_title":"FROM","season_number":4,"episode_number":2,"watch":{"position_ms":600000,"duration_ms":3240000}}"#.utf8)
        )
        let movie = try decoder.decode(
            Item.self,
            from: Data(#"{"id":2,"kind":"movie","title":"TRON: Ares","year":2025,"watch":{"position_ms":300000,"duration_ms":7200000}}"#.utf8)
        )

        XCTAssertEqual(continueWatchingDetail(episode), "S4 E2 · Fray")
        XCTAssertEqual(continueWatchingTimeRemaining(episode), "44m left")
        XCTAssertEqual(continueWatchingDetail(movie), "2025")
        XCTAssertEqual(continueWatchingTimeRemaining(movie), "115m left")
    }

    private func contrastRatio(_ foreground: Color, _ background: Color) -> CGFloat {
        let lighter = max(relativeLuminance(foreground), relativeLuminance(background))
        let darker = min(relativeLuminance(foreground), relativeLuminance(background))
        return (lighter + 0.05) / (darker + 0.05)
    }

    private func relativeLuminance(_ color: Color) -> CGFloat {
        var red: CGFloat = 0
        var green: CGFloat = 0
        var blue: CGFloat = 0
        var alpha: CGFloat = 0
        XCTAssertTrue(UIColor(color).getRed(&red, green: &green, blue: &blue, alpha: &alpha))

        func linear(_ component: CGFloat) -> CGFloat {
            component <= 0.04045
                ? component / 12.92
                : pow((component + 0.055) / 1.055, 2.4)
        }

        return 0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }
    #endif

    func testNativeItemDecodesLibraryProvenanceAndSortFields() throws {
        let json = #"{"id":7,"library_id":12,"kind":"movie","title":"Feature","added_at":99,"updated_at":101}"#
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase

        let item = try decoder.decode(Item.self, from: Data(json.utf8))

        XCTAssertEqual(item.libraryId, 12)
        XCTAssertEqual(item.addedAt, 99)
        XCTAssertEqual(item.updatedAt, 101)
    }

    func testWatchFiltersMatchWebClientSemantics() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        func item(_ watch: String) throws -> Item {
            let json = "{\"id\":1,\"kind\":\"movie\",\"title\":\"Feature\",\"watch\":\(watch)}"
            return try decoder.decode(Item.self, from: Data(json.utf8))
        }
        let unwatched = try item("{\"position_ms\":0,\"watched\":false}")
        let progressing = try item("{\"position_ms\":4000,\"watched\":false}")
        let watched = try item("{\"position_ms\":0,\"watched\":true}")

        XCTAssertTrue(AppModel.matches(unwatched, filter: .unwatched))
        XCTAssertTrue(AppModel.matches(progressing, filter: .inProgress))
        XCTAssertTrue(AppModel.matches(watched, filter: .watched))
        XCTAssertFalse(AppModel.matches(progressing, filter: .unwatched))
    }

    func testPluralServerLibraryKindsMapToNativeMovieAndTVTabs() {
        XCTAssertEqual(AppModel.canonicalLibraryKind("movies"), "movie")
        XCTAssertEqual(AppModel.canonicalLibraryKind("shows"), "show")
        XCTAssertEqual(AppModel.canonicalLibraryKind("home"), "home")
        XCTAssertEqual(AppModel.canonicalLibraryKind("MOVIES"), "movie")
    }

    func testPosterSizesAreOrderedAndMatchTheWebChoices() {
        XCTAssertEqual(PosterSize.allCases.map(\.label), ["Small", "Medium", "Large", "Extra large"])
        XCTAssertLessThan(PosterSize.small.posterWidth, PosterSize.medium.posterWidth)
        XCTAssertLessThan(PosterSize.medium.posterWidth, PosterSize.large.posterWidth)
        XCTAssertLessThan(PosterSize.large.posterWidth, PosterSize.extraLarge.posterWidth)
    }

    func testCancelledURLRequestRemainsCancellationInsteadOfConnectionFailure() {
        let mapped = PlurxAPI.transportError(from: URLError(.cancelled))

        XCTAssertTrue(mapped is CancellationError)
        XCTAssertNil(AppModel.homeErrorMessage(for: mapped, hasCachedContent: false))
    }

    func testTransientRefreshFailureKeepsCachedHomeContentVisible() {
        let failure = APIError.transport("The request timed out.")

        XCTAssertNil(AppModel.homeErrorMessage(for: failure, hasCachedContent: true))
        XCTAssertEqual(
            AppModel.homeErrorMessage(for: failure, hasCachedContent: false),
            "The request timed out."
        )
    }
}
