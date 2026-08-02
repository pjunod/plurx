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

    /// Pins every row of the standing subtitle rule: automatic selection never
    /// starts a burn, except a forced track, which may. Rows that return nil
    /// are the point — an encoder slot per play, H.264 SDR, HDR gone, is what
    /// the deleted `?? matching.first` tail used to buy.
    @MainActor
    func testAutomaticSubtitlePolicyPinsAllFourRowsOfTheSubtitleRule() {
        // Row 1 — forced, whatever the codec. A forced PGS is the one burn
        // automatic selection is allowed to start, because a film whose foreign
        // dialogue is unsubtitled is not watchable at all.
        let forcedBitmap = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Forced",
                default: false, forced: true, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(forcedBitmap, preferredLanguage: "eng"),
            0,
            "forced-PGS is the one permitted automatic burn"
        )
        // Both forced signals reach the same carve-out: some muxes set only the
        // title.
        let titleOnlyForcedBitmap = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Forced Narrative",
                default: false, forced: false, text: false
            ),
        ]
        XCTAssertTrue(PlayerController.isForcedSubtitle(titleOnlyForcedBitmap[0]))
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(titleOnlyForcedBitmap, preferredLanguage: "eng"),
            0
        )

        // Row 2 — default-flagged native text goes on through the free
        // rendition path: no encoder, no restart.
        let defaultText = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "English",
                default: true, forced: false, text: true
            ),
        ]
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(defaultText, preferredLanguage: "eng"),
            0
        )

        // Row 3 — the standard 4K disc remux: the only English track is a
        // default-flagged PGS. Cold start stays a copy.
        let defaultBitmapOnly = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: true, forced: false, text: false
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(defaultBitmapOnly, preferredLanguage: "eng"),
            "a default-flagged bitmap track must never cold-start a burn transcode"
        )
        let defaultStyled = [
            SubtitleTrack(
                index: 0, codec: "ass", language: "eng", title: "Signs",
                default: true, forced: false, text: false
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(defaultStyled, preferredLanguage: "eng"),
            "styled ASS cannot become a WebVTT rendition, so it burns — never automatically"
        )

        // Row 4 — merely the same language. This is the deleted tail: an
        // unflagged English track no longer captions every English film, and an
        // unflagged English PGS no longer transcodes one.
        let unflaggedText = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "English",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "SDH",
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(unflaggedText, preferredLanguage: "eng")
        )
        let unflaggedBitmapOnly = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: false, forced: false, text: false
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(unflaggedBitmapOnly, preferredLanguage: "eng"),
            "unflagged English PGS only: cold start must attach no encoder"
        )

        // The language guard survives the rewrite: a flagged Italian default
        // never captions an English-audio film.
        let italianDefault = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "ita", title: "Italiano",
                default: true, forced: false, text: true
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(italianDefault, preferredLanguage: "eng")
        )
    }

    /// The no-overlap guard stays (two replacements share a `playback_id`), but
    /// intent that lands during one is now queued rather than dropped.
    func testASeekDuringAStreamChangeIsQueuedAndReplayedExactlyOnce() {
        var queue = PlayerReopenQueue()

        // Nothing in flight: the request opens immediately and queues nothing.
        let immediate = queue.request(10_000, changeInFlight: false)
        XCTAssertEqual(immediate, 10_000)
        XCTAssertNil(queue.pendingMs)

        // A burst of tvOS 30-second step-seeks landing during that change. Each
        // is remembered instead of vanishing, the newest wins, and none of them
        // starts a second, overlapping replacement.
        let firstStep = queue.request(40_000, changeInFlight: true)
        let secondStep = queue.request(70_000, changeInFlight: true)
        let thirdStep = queue.request(100_000, changeInFlight: true)
        XCTAssertNil(firstStep)
        XCTAssertNil(secondStep)
        XCTAssertNil(thirdStep)
        XCTAssertEqual(queue.pendingMs, 100_000)

        // Exactly one trailing reopen, at the position of the last press —
        // not the one the change started from.
        let trailing = queue.takePending()
        let secondTrailing = queue.takePending()
        XCTAssertEqual(trailing, 100_000)
        XCTAssertNil(secondTrailing, "the trailing reopen runs once, not once per request")

        // A track change naming the position already being opened must still
        // reopen: it is the session recipe that has to change, not the timeline.
        let trackChangeStart = queue.request(100_000, changeInFlight: false)
        let queuedTrackChange = queue.request(100_000, changeInFlight: true)
        let trackChangeTrailing = queue.takePending()
        XCTAssertEqual(trackChangeStart, 100_000)
        XCTAssertNil(queuedTrackChange)
        XCTAssertEqual(
            trackChangeTrailing,
            100_000,
            "a queued request at the same position still has to rebuild the session"
        )

        // A failed change, or a stopped player, drops the queue rather than
        // reopening a stream that is already gone.
        let orphaned = queue.request(5_000, changeInFlight: true)
        XCTAssertNil(orphaned)
        queue.clear()
        XCTAssertNil(queue.pendingMs)
        let afterClear = queue.takePending()
        XCTAssertNil(afterClear)
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

    /// Both halves of the Settings toggle, which is the only thing that reads
    /// `SubtitleReadiness`. `.instant` is the shipped default and must keep
    /// answering exactly as the old `!hasNativeSubtitles` guard did.
    @MainActor
    func testSubtitleReadinessDecidesWhetherAPlayInvolvesTheServerAtAll() {
        // `.instant`: a native text track anywhere in the file is enough, and
        // it is enough before anyone has opened the subtitle menu.
        XCTAssertTrue(PlayerController.needsNativeSubtitleSession(
            hasNativeTextTrack: true,
            readiness: .instant,
            subtitlesInUse: false
        ))
        XCTAssertTrue(PlayerController.needsNativeSubtitleSession(
            hasNativeTextTrack: true,
            readiness: .instant,
            subtitlesInUse: true
        ))

        // `.onDemand`: the same file direct-plays until a native track is
        // actually asked for. This is the play the server never hears about.
        XCTAssertFalse(PlayerController.needsNativeSubtitleSession(
            hasNativeTextTrack: true,
            readiness: .onDemand,
            subtitlesInUse: false
        ))
        XCTAssertTrue(
            PlayerController.needsNativeSubtitleSession(
                hasNativeTextTrack: true,
                readiness: .onDemand,
                subtitlesInUse: true
            ),
            "once a text track is in use the reopen has to be a session, or there is nothing to select"
        )

        // No native text track: neither setting can invent renditions, so a
        // PGS-only or mov_text-only file direct-plays under both. This row is
        // why a bitmap track cannot cost a direct play.
        for readiness in SubtitleReadiness.allCases {
            XCTAssertFalse(PlayerController.needsNativeSubtitleSession(
                hasNativeTextTrack: false,
                readiness: readiness,
                subtitlesInUse: false
            ))
            XCTAssertFalse(PlayerController.needsNativeSubtitleSession(
                hasNativeTextTrack: false,
                readiness: readiness,
                subtitlesInUse: true
            ))
        }
    }

    /// A fresh install, and an install that has never touched the new control,
    /// must both behave exactly as v0.2 shipped.
    func testSubtitleReadinessDefaultsToTodaysBehaviorAndPersistsAChange() throws {
        let suite = "tv.plurx.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let fresh = SettingsStore(defaults: defaults)
        XCTAssertEqual(fresh.subtitleReadiness, .instant)

        // An unreadable or future value falls back to the default rather than
        // silently changing how titles open.
        defaults.set("something-else", forKey: "plurx.subtitleReadiness")
        XCTAssertEqual(SettingsStore(defaults: defaults).subtitleReadiness, .instant)

        fresh.subtitleReadiness = .onDemand
        XCTAssertEqual(SettingsStore(defaults: defaults).subtitleReadiness, .onDemand)
    }

    /// Which subtitle selections have to rebuild the stream. Under `.onDemand`
    /// the first native pick during direct play is a new member of that set —
    /// a raw file URL has no renditions to select — and it takes the same clean
    /// reopen a burn already takes, at `realPositionMs()`.
    @MainActor
    func testFirstSubtitleChoiceDuringDirectPlayRebuildsTheStreamOnceAndNoMore() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "eng", title: "English",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false
            ),
        ]

        // Direct play, nothing burned: the first native pick is the restart.
        XCTAssertTrue(PlayerController.subtitleSelectionRequiresReopen(
            index: 0, tracks: tracks, hasActiveBurn: false, isDirectPlayback: true
        ))
        // Turning subtitles off during direct play restarts nothing — there was
        // never anything on.
        XCTAssertFalse(PlayerController.subtitleSelectionRequiresReopen(
            index: nil, tracks: tracks, hasActiveBurn: false, isDirectPlayback: true
        ))
        // Once the copy session exists, every further text switch is free
        // again: this is the second-and-later selection, and it must not
        // reopen.
        XCTAssertFalse(PlayerController.subtitleSelectionRequiresReopen(
            index: 0, tracks: tracks, hasActiveBurn: false, isDirectPlayback: false
        ))

        // The pre-existing reasons are untouched: entering a burn, and leaving
        // one, still reopen whatever the delivery mode.
        XCTAssertTrue(PlayerController.subtitleSelectionRequiresReopen(
            index: 1, tracks: tracks, hasActiveBurn: false, isDirectPlayback: false
        ))
        XCTAssertTrue(PlayerController.subtitleSelectionRequiresReopen(
            index: nil, tracks: tracks, hasActiveBurn: true, isDirectPlayback: false
        ))
        XCTAssertTrue(PlayerController.subtitleSelectionRequiresReopen(
            index: 0, tracks: tracks, hasActiveBurn: true, isDirectPlayback: false
        ))
        // An index the decision never listed is treated as a burn, as before.
        XCTAssertTrue(PlayerController.subtitleSelectionRequiresReopen(
            index: 99, tracks: tracks, hasActiveBurn: false, isDirectPlayback: false
        ))
    }

    /// The 23-`mov_text` MP4: every track is `text`, none is `native`. Before
    /// the server sent `native`, the codec list happened to agree — the point
    /// of this test is that the client now takes the server's answer, so the
    /// two can never disagree about which tracks are in the HLS master.
    func testServerNativeFlagDecidesRenditionsAndOverridesTheLocalCodecGuess() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase

        let movText = try decoder.decode(SubtitleTrack.self, from: Data(#"""
        {"index":0,"codec":"mov_text","language":"eng","title":"English",
         "default":true,"forced":false,"text":true,"native":false}
        """#.utf8))
        XCTAssertTrue(movText.text, "mov_text is extractable text")
        XCTAssertFalse(movText.isNativeHLS, "…and still cannot be an HLS rendition")

        // A server that predates the field decodes to nil and falls back to the
        // local codec check, which is what shipped.
        let legacy = try decoder.decode(SubtitleTrack.self, from: Data(#"""
        {"index":1,"codec":"subrip","language":"eng","default":false,
         "forced":false,"text":true}
        """#.utf8))
        XCTAssertNil(legacy.native)
        XCTAssertTrue(legacy.isNativeHLS)

        // And where the two could disagree, the server wins in both directions:
        // a codec this client has never heard of that the server can publish,
        // and a codec it would have published that the server will not.
        let serverSaysYes = try decoder.decode(SubtitleTrack.self, from: Data(#"""
        {"index":2,"codec":"stl","default":false,"forced":false,
         "text":true,"native":true}
        """#.utf8))
        XCTAssertTrue(serverSaysYes.isNativeHLS)
        let serverSaysNo = try decoder.decode(SubtitleTrack.self, from: Data(#"""
        {"index":3,"codec":"subrip","default":false,"forced":false,
         "text":true,"native":false}
        """#.utf8))
        XCTAssertFalse(serverSaysNo.isNativeHLS)
    }

    /// Ordinals and the §3.1 automatic policy must read the *same* notion of
    /// "can be a rendition", or a `mov_text` or ASS track silently shifts the
    /// rendition a viewer's pick resolves to and AVPlayer captions the wrong
    /// language.
    @MainActor
    func testTextButNotNativeTracksBurnAndNeverShiftARenditionOrdinal() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "mov_text", language: "eng", title: "English",
                default: true, forced: false, text: true, native: false
            ),
            SubtitleTrack(
                index: 1, codec: "ass", language: "eng", title: "Signs",
                default: false, forced: false, text: true, native: false
            ),
            SubtitleTrack(
                index: 2, codec: "subrip", language: "eng", title: "English SDH",
                default: false, forced: false, text: true, native: true
            ),
            SubtitleTrack(
                index: 3, codec: "hdmv_pgs_subtitle", language: "eng", title: "PGS",
                default: false, forced: false, text: false, native: false
            ),
            SubtitleTrack(
                index: 4, codec: "webvtt", language: "fre", title: "Français",
                default: false, forced: false, text: true, native: true
            ),
        ]

        // Only the two native tracks are in the master, in source order. If
        // `text` were the test, the mov_text and ASS tracks would count too:
        // index 2 would resolve to rendition 2 and index 4 to rendition 3, of a
        // master that carries exactly two — so the viewer's pick would land on
        // the wrong track or on nothing at all.
        XCTAssertEqual(PlayerController.nativeSubtitleOrdinal(2, in: tracks), 0)
        XCTAssertEqual(PlayerController.nativeSubtitleOrdinal(4, in: tracks), 1)
        XCTAssertNil(PlayerController.nativeSubtitleOrdinal(0, in: tracks))
        XCTAssertNil(PlayerController.nativeSubtitleOrdinal(1, in: tracks))
        XCTAssertNil(PlayerController.nativeSubtitleOrdinal(3, in: tracks))

        // Text-but-not-native routes to burn rather than to a rendition that
        // would be answered with 400.
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(0, in: tracks))
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(1, in: tracks))
        XCTAssertFalse(PlayerController.subtitleRequiresBurn(2, in: tracks))

        // §3.1: a default-flagged `mov_text` track is not a permitted cold
        // start. Index 0 is default-flagged and English; taking it would ask
        // for a rendition the master does not contain. Nothing else here is
        // flagged, so automatic selection declines entirely — which is the
        // whole rule, not a failure.
        XCTAssertNil(PlayerController.automaticSubtitleIndex(tracks, preferredLanguage: "eng"))

        // The same file with the native track flagged instead: now automatic
        // selection has something free to take.
        let flagged = [
            SubtitleTrack(
                index: 0, codec: "mov_text", language: "eng", title: "English",
                default: false, forced: false, text: true, native: false
            ),
            SubtitleTrack(
                index: 2, codec: "subrip", language: "eng", title: "English SDH",
                default: true, forced: false, text: true, native: true
            ),
        ]
        XCTAssertEqual(PlayerController.automaticSubtitleIndex(flagged, preferredLanguage: "eng"), 2)

        // The forced carve-out is unchanged and still reaches a burn: a forced
        // `mov_text` track is dialogue the film needs, so it burns at source
        // height rather than being dropped.
        let forcedMovText = [
            SubtitleTrack(
                index: 0, codec: "mov_text", language: "eng", title: "Forced",
                default: true, forced: true, text: true, native: false
            ),
        ]
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(forcedMovText, preferredLanguage: "eng"),
            0
        )
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(0, in: forcedMovText))
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

    /// Origin and bearer are written together or not at all. The failure this
    /// pins is a real one: killed between connecting to B and signing in, the
    /// old build relaunched holding server A's bearer next to server B's
    /// address, sent it in cleartext, and destroyed A's still-valid session the
    /// moment B answered 401.
    func testChangingTheServerClearsThePersistedTokenInTheSameWrite() throws {
        final class TokenStoreDouble: TokenStoring {
            var value: String?
            func read() -> String? { value }
            func write(_ token: String) -> Bool { value = token; return true }
            func clear() { value = nil }
        }

        let suite = "tv.plurx.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let vault = TokenStoreDouble()
        let settings = SettingsStore(defaults: defaults, tokenVault: vault)

        // Signed in to server A.
        settings.setServer(origin: "http://a:32400", instanceId: "server-a", token: nil)
        settings.token = "server-a-bearer"
        XCTAssertEqual(settings.token, "server-a-bearer")

        // Connect to B, then lose the process before the login can complete.
        settings.setServer(origin: "http://b:32400", instanceId: "server-b", token: nil)

        // A fresh launch reads storage: B's address, and nothing to authorize
        // with — so no `Authorization` header can be built for B at all.
        let relaunched = SettingsStore(defaults: defaults, tokenVault: vault)
        XCTAssertEqual(relaunched.origin, "http://b:32400")
        XCTAssertEqual(relaunched.instanceId, "server-b")
        XCTAssertNil(relaunched.token, "server A's bearer must never reach server B")
        XCTAssertNil(defaults.string(forKey: "plurx.token"))
        XCTAssertNil(vault.value)

        // The same instance rediscovered at a new address is a move, not a
        // change of identity: its own token travels with it, every time.
        relaunched.setServer(
            origin: "http://b2:32400", instanceId: "server-b", token: "server-b-bearer"
        )
        XCTAssertEqual(relaunched.token, "server-b-bearer")
        relaunched.setServer(
            origin: "http://b3:32400", instanceId: "server-b", token: "server-b-bearer"
        )
        XCTAssertEqual(relaunched.token, "server-b-bearer")

        // Leaving a server entirely takes address, identity, and bearer.
        relaunched.clearServer()
        XCTAssertEqual(relaunched.origin, "")
        XCTAssertNil(relaunched.instanceId)
        XCTAssertNil(relaunched.token)
        XCTAssertNil(vault.value)
    }

    /// The value of this test is that it *finishes*. `NetService` delivers every
    /// outcome — including its own `withTimeout:` expiry — through run-loop
    /// sources, so resolving from a cooperative-pool thread could never call
    /// back: the continuation leaked and `await` never returned, wedging the
    /// connect screen and saved-session recovery behind a permanent spinner.
    @MainActor
    func testBonjourResolutionOfAMissingServiceFailsInsteadOfHangingTheCaller() async {
        let resolver = BonjourResolver(
            name: "plurx-absent-\(UUID().uuidString)",
            type: PlurxClientDefaults.bonjourServiceType,
            domain: "local."
        )
        let started = Date()

        do {
            let origin = try await resolver.resolve(timeout: 1)
            XCTFail("a service that does not exist must not resolve; got \(origin)")
        } catch {
            XCTAssertTrue(
                error is ServerDiscoveryError,
                "resolution failure must surface as a discovery error, got \(error)"
            )
        }

        XCTAssertLessThan(
            Date().timeIntervalSince(started),
            5,
            "resolve must return on its own deadline rather than outlive the caller"
        )
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

    /// The three states of MEDIA-BADGES-PLAN §2.3 on one DV Profile 8 remux.
    /// The badge text always starts from what the *file* carries — that claim
    /// stays true either way — and the arrow says what is actually reaching the
    /// screen. Nothing here is allowed to influence a decision or a session
    /// request: this whole function is a readout.
    func testDynamicRangeBadgeNamesWhatIsDeliveredNotOnlyWhatTheFileCarries() throws {
        let dolbyVision = SourceSummary(
            container: "mkv", videoCodec: "hevc", videoProfile: nil,
            width: 3_840, height: 2_160, bitDepth: 10,
            hdr: "dolby_vision", hdrFormat: "Dolby Vision · Profile 8 (HDR10-compatible)",
            bitrate: nil, durationMs: nil
        )

        func badge(_ delivered: String?, displayHDR: Bool = true) throws -> PlayerMetadataBadge {
            try XCTUnwrap(PlayerView.dynamicRangeBadge(
                hdr: dolbyVision.hdr,
                hdrFormat: dolbyVision.hdrFormat,
                delivered: delivered,
                displayHDR: displayHDR
            ))
        }

        // Source-only: no session yet, or a server too old to report. Exactly
        // the chip this client has always drawn.
        let sourceOnly = try badge(nil)
        XCTAssertEqual(sourceOnly.mark, "DV")
        XCTAssertFalse(sourceOnly.dimmed)

        // Lit: the copy session kept the RPUs and the display can show them.
        let lit = try badge("dolby_vision")
        XCTAssertEqual(lit.mark, "DV")
        XCTAssertEqual(lit.accessibilityLabel, "Dolby Vision")
        XCTAssertFalse(lit.dimmed)

        // Downgraded by the server: an unclaimed profile takes the strip path,
        // which delivers the compatible HDR10 base.
        let stripped = try badge("hdr10")
        XCTAssertEqual(stripped.mark, "DV → HDR10")
        XCTAssertEqual(stripped.accessibilityLabel, "Dolby Vision, playing as HDR10")
        XCTAssertTrue(stripped.dimmed)

        // Downgraded by the transcode: a burn or a picked rung is H.264 8-bit.
        let transcoded = try badge("sdr")
        XCTAssertEqual(transcoded.mark, "DV → SDR")
        XCTAssertTrue(transcoded.dimmed)

        // Downgraded by the display: delivered bits are necessary, not
        // sufficient. This is the whole of what the client is allowed to know
        // about rendering — no headroom polling, no variant introspection.
        let sdrPanel = try badge("dolby_vision", displayHDR: false)
        XCTAssertEqual(sdrPanel.mark, "DV → SDR")
        XCTAssertTrue(sdrPanel.dimmed)

        // A plain HDR10 source keeps the terse base mark and reports its own
        // losses; an SDR source has no grade to report on at all.
        let hdr10 = try XCTUnwrap(PlayerView.dynamicRangeBadge(
            hdr: "hdr10", hdrFormat: "HDR10", delivered: "sdr", displayHDR: true
        ))
        XCTAssertEqual(hdr10.mark, "HDR → SDR")
        XCTAssertEqual(hdr10.accessibilityLabel, "HDR, playing as SDR")
        XCTAssertNil(PlayerView.dynamicRangeBadge(
            hdr: nil, hdrFormat: nil, delivered: "sdr", displayHDR: true
        ))
    }

    /// The badge row as a whole: the dynamic-range chip is the only one this
    /// pass may touch. Resolution and audio stay source-only by design
    /// (MEDIA-BADGES-PLAN §9), and the defaulted arguments keep every
    /// pre-existing caller on the source-only path.
    func testDeliveredRangeChangesOnlyTheDynamicRangeBadge() {
        let source = SourceSummary(
            container: nil, videoCodec: "hevc", videoProfile: nil,
            width: nil, height: 2_160, bitDepth: nil,
            hdr: nil, hdrFormat: "Dolby Vision · Profile 7 (HDR10-compatible)",
            bitrate: nil, durationMs: nil
        )
        let audio = AudioTrack(
            index: 0, codec: "truehd", channels: 8,
            language: "eng", title: "Atmos", default: true
        )

        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.mark),
            [nil, "DV", "ATMOS 7.1"]
        )
        let downgraded = PlayerView.playbackBadges(
            source: source, audio: audio, delivered: "sdr", displayHDR: true
        )
        XCTAssertEqual(downgraded.map(\.mark), [nil, "DV → SDR", "ATMOS 7.1"])
        XCTAssertEqual(downgraded.map(\.dimmed), [false, true, false])
        XCTAssertEqual(
            downgraded.map(\.symbol),
            ["4k.tv.fill", "sparkles", "waveform"]
        )
        XCTAssertLessThanOrEqual(PlayerMetadataBadgeMetrics.dimmedOpacity, 0.5)
        XCTAssertGreaterThan(PlayerMetadataBadgeMetrics.dimmedOpacity, 0.2)
    }

    /// The playback-info panel says the same thing in a sentence, and prefers
    /// the server's own reason over anything the client could invent.
    func testDynamicRangePanelRowExplainsWhyTheGradeChanged() {
        let source = SourceSummary(
            container: nil, videoCodec: "hevc", videoProfile: nil,
            width: 3_840, height: 2_160, bitDepth: 10,
            hdr: "dolby_vision", hdrFormat: "Dolby Vision · Profile 7 (HDR10-compatible)",
            bitrate: nil, durationMs: nil
        )
        let reason = "Dolby Vision metadata removed for this device; compatible HDR base kept"

        XCTAssertEqual(
            PlayerView.dynamicRangeSummary(
                source: source, delivered: "dolby_vision",
                displayHDR: true, reasons: [reason]
            ),
            "Dolby Vision (rendering)"
        )
        XCTAssertEqual(
            PlayerView.dynamicRangeSummary(
                source: source, delivered: "hdr10", displayHDR: true, reasons: [reason]
            ),
            "HDR10 — \(reason)"
        )
        XCTAssertEqual(
            PlayerView.dynamicRangeSummary(
                source: source, delivered: "dolby_vision", displayHDR: false, reasons: [reason]
            ),
            "SDR — this display is not HDR"
        )
        XCTAssertEqual(
            PlayerView.dynamicRangeSummary(
                source: source, delivered: "sdr", displayHDR: true, reasons: ["Container not supported"]
            ),
            "SDR — tone-mapped from Dolby Vision"
        )
        // No session and no report: the rich source label, and nothing invented.
        XCTAssertEqual(
            PlayerView.dynamicRangeSummary(
                source: source, delivered: nil, displayHDR: true, reasons: nil
            ),
            "Dolby Vision · Profile 7 (HDR10-compatible)"
        )
        XCTAssertNil(PlayerView.dynamicRangeSummary(
            source: nil, delivered: nil, displayHDR: true, reasons: nil
        ))
    }

    /// Source grades collapse to the server's own vocabulary so that a source
    /// and `delivered_dynamic_range` compare by string equality. Files probed
    /// before `hdr` existed carry only the rich label, so both fields are read.
    func testSourceGradeCollapsesToTheServersVocabulary() {
        XCTAssertEqual(
            PlayerView.playbackBadges(source: nil, audio: nil).count,
            0
        )
        XCTAssertEqual(DynamicRange.source(hdr: "dolby_vision", hdrFormat: nil), "dolby_vision")
        XCTAssertEqual(
            DynamicRange.source(hdr: nil, hdrFormat: "Dolby Vision · Profile 5"),
            "dolby_vision"
        )
        XCTAssertEqual(DynamicRange.source(hdr: "hdr10", hdrFormat: "HDR10+"), "hdr10")
        XCTAssertEqual(DynamicRange.source(hdr: nil, hdrFormat: "HDR10+"), "hdr10")
        XCTAssertEqual(DynamicRange.source(hdr: nil, hdrFormat: "HLG"), "hlg")
        XCTAssertEqual(DynamicRange.source(hdr: "hlg", hdrFormat: nil), "hlg")
        XCTAssertNil(DynamicRange.source(hdr: nil, hdrFormat: nil))
        XCTAssertNil(DynamicRange.source(hdr: "sdr", hdrFormat: nil))
        XCTAssertNil(DynamicRange.source(hdr: "", hdrFormat: "  "))
    }

    /// The wire fields M4 consumes. Both are optional in Swift because both can
    /// be absent — an older server on the decision, and a session whose source
    /// row could not be read on the start response.
    func testDeliveredDynamicRangeDecodesFromBothResponses() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase

        let decision = try decoder.decode(Decision.self, from: Data(#"""
        {"file_id":6045,"method":"remux","play_url":"/api/v1/files/6045/direct",
         "preserve_dolby_vision":true,"delivered_dynamic_range":"dolby_vision"}
        """#.utf8))
        XCTAssertEqual(decision.deliveredDynamicRange, "dolby_vision")

        let session = try decoder.decode(HlsStart.self, from: Data(#"""
        {"session_id":"s1","playlist_url":"/hls/s1/master.m3u8",
         "delivered_dynamic_range":"sdr"}
        """#.utf8))
        XCTAssertEqual(session.deliveredDynamicRange, "sdr")

        // An older server: the field is simply absent, and the badge falls back
        // to source-only rather than failing to decode.
        let legacy = try decoder.decode(HlsStart.self, from: Data(#"""
        {"session_id":"s2","playlist_url":"/hls/s2/master.m3u8"}
        """#.utf8))
        XCTAssertNil(legacy.deliveredDynamicRange)
    }

    /// The detail page had no dynamic-range badge at all, while Android and the
    /// web both did. It is source-only and stays that way: there is no session
    /// on a detail page to report a downgrade against.
    func testDetailBadgesCarryTheSourceDynamicRangeAfterTheCodec() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let item = try decoder.decode(
            Item.self,
            from: Data(#"{"id":6045,"kind":"movie","title":"Feature","year":1994}"#.utf8)
        )
        let file = try decoder.decode(MediaFile.self, from: Data(#"""
        {"id":11,"duration_ms":8520000,"container":"mkv","video_codec":"hevc",
         "width":3840,"height":2160,"hdr":"dolby_vision",
         "hdr_format":"Dolby Vision · Profile 8 (HDR10-compatible)"}
        """#.utf8))

        let badges = DetailView.itemMetadataBadges(
            item, file: file, durationMs: file.durationMs, includeSeries: false
        )
        XCTAssertEqual(badges.map(\.kind), [.year, .runtime, .resolution, .video, .dynamicRange])
        let range = try XCTUnwrap(badges.last)
        XCTAssertEqual(range.symbol, "sparkles")
        XCTAssertEqual(range.mark, "DV")
        XCTAssertEqual(range.accessibilityLabel, "Dolby Vision")

        // An SDR file gains nothing, exactly as before.
        let sdr = try decoder.decode(MediaFile.self, from: Data(#"""
        {"id":12,"duration_ms":8520000,"container":"mp4","video_codec":"h264","height":1080}
        """#.utf8))
        XCTAssertEqual(
            DetailView.itemMetadataBadges(
                item, file: sdr, durationMs: sdr.durationMs, includeSeries: false
            ).map(\.kind),
            [.year, .runtime, .resolution, .video]
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

    /// A show has no watch row: the state lives on its episodes, which a
    /// library page does not contain. Before the server started attaching
    /// `rollup` to containers, a TV grid's "Watched" and "In progress" filtered
    /// to nothing at all and "Unwatched" listed finished series — this pins the
    /// three buckets the acceptance case asks for, on one page of shows.
    func testShowLibraryFiltersClassifyContainersFromTheirRollup() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        func show(_ id: Int, _ rollup: String) throws -> Item {
            try decoder.decode(Item.self, from: Data(
                "{\"id\":\(id),\"kind\":\"show\",\"title\":\"Series\",\"rollup\":\(rollup)}".utf8
            ))
        }
        let finished = try show(1, "{\"leaves\":24,\"watched\":24}")
        let halfWatched = try show(2, "{\"leaves\":24,\"watched\":11}")
        let untouched = try show(3, "{\"leaves\":24,\"watched\":0}")

        XCTAssertEqual(AppModel.watchState(of: finished), .watched)
        XCTAssertEqual(AppModel.watchState(of: halfWatched), .inProgress)
        XCTAssertEqual(AppModel.watchState(of: untouched), .unwatched)

        let page = [finished, halfWatched, untouched]
        XCTAssertEqual(page.filter { AppModel.matches($0, filter: .watched) }.map(\.id), [1])
        XCTAssertEqual(page.filter { AppModel.matches($0, filter: .inProgress) }.map(\.id), [2])
        XCTAssertEqual(page.filter { AppModel.matches($0, filter: .unwatched) }.map(\.id), [3])
        XCTAssertEqual(page.filter { AppModel.matches($0, filter: .all) }.count, 3)

        // A container the server could not roll up (no episodes yet) is not
        // silently called watched: an empty rollup falls through to `watch`.
        let empty = try show(4, "{\"leaves\":0,\"watched\":0}")
        XCTAssertEqual(AppModel.watchState(of: empty), .unwatched)

        // Leaves are untouched by any of this — they still answer from `watch`.
        let episode = try decoder.decode(Item.self, from: Data(
            #"{"id":5,"kind":"episode","title":"Fray","watch":{"position_ms":600000,"watched":false}}"#.utf8
        ))
        XCTAssertEqual(AppModel.watchState(of: episode), .inProgress)
    }

    /// After bootstrap, a rotated or revoked bearer has to end the session
    /// rather than degrade every screen to "Server returned 401" while the app
    /// still looks signed in. The classification is the testable half; the
    /// effect (`clearToken` + `.needLogin`) hangs off it in `noteAuthFailure`.
    func testRevokedTokenIsRecognizedAsAnExpiredSessionAndNothingElseIs() {
        XCTAssertTrue(AppModel.isSessionExpired(APIError.http(401)))
        XCTAssertTrue(AppModel.isSessionExpired(APIError.http(403)))
        XCTAssertFalse(AppModel.isSessionExpired(APIError.http(404)))
        XCTAssertFalse(AppModel.isSessionExpired(APIError.http(500)))
        XCTAssertFalse(AppModel.isSessionExpired(APIError.transport("The request timed out.")))
        XCTAssertFalse(AppModel.isSessionExpired(APIError.badURL))
        XCTAssertFalse(AppModel.isSessionExpired(CancellationError()))
        XCTAssertFalse(AppModel.isSessionExpired(URLError(.notConnectedToInternet)))
    }

    /// Posters are decoded to the cell that will draw them, not to their full
    /// raster. A 400×600 source through a 120-pixel ceiling comes back inside
    /// that ceiling with its aspect intact — the whole reason a tvOS
    /// `.extraLarge` grid stops decoding megabytes per cell.
    func testPosterArtworkDecodesDownToTheCellThatWillDrawIt() throws {
        let format = UIGraphicsImageRendererFormat.preferredFormat()
        format.scale = 1
        let renderer = UIGraphicsImageRenderer(
            size: CGSize(width: 400, height: 600),
            format: format
        )
        let data = renderer.pngData { context in
            UIColor.red.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 400, height: 600))
        }

        let downsampled = try XCTUnwrap(AuthImageCache.downsample(data, maxPixelSize: 120))
        XCTAssertLessThanOrEqual(max(downsampled.size.width, downsampled.size.height), 120)
        XCTAssertGreaterThan(max(downsampled.size.width, downsampled.size.height), 60)
        XCTAssertLessThan(downsampled.size.width, downsampled.size.height)

        // Bytes that are not an image stay nil rather than becoming a blank
        // placeholder that never retries.
        XCTAssertNil(AuthImageCache.downsample(Data("not an image".utf8), maxPixelSize: 120))

        // Cache identity: the same path is a different picture on another
        // server, and the same picture is a different decode at another size.
        let poster = AuthImageCache.key(origin: "http://a:32400", path: "/i/7", maxPixelSize: 300)
        XCTAssertNotEqual(
            poster,
            AuthImageCache.key(origin: "http://b:32400", path: "/i/7", maxPixelSize: 300)
        )
        XCTAssertNotEqual(
            poster,
            AuthImageCache.key(origin: "http://a:32400", path: "/i/7", maxPixelSize: 900)
        )
        XCTAssertEqual(
            poster,
            AuthImageCache.key(origin: "http://a:32400", path: "/i/7", maxPixelSize: 300)
        )
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
