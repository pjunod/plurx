import AVFoundation
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

private struct NativeAPIContractFixture: Decodable {
    let server: ServerInfo
    let itemDetail: ItemDetail
    let page: Page
    let decision: Decision
}

private actor ArtworkDownloadProbe {
    private var starts = 0
    private var cancellations = 0

    func runUntilCancelled() async -> AuthImageDownloadResult {
        starts += 1
        do {
            try await Task.sleep(nanoseconds: 5_000_000_000)
            return .success(Data())
        } catch {
            cancellations += 1
            return .transientFailure
        }
    }

    func snapshot() -> (starts: Int, cancellations: Int) {
        (starts, cancellations)
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

    func testSharedNativeAPIFixtureDecodesWithoutConsumerDrift() throws {
        let fixtureURL = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("tests/contracts/native-api.json")
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let fixture = try decoder.decode(
            NativeAPIContractFixture.self,
            from: Data(contentsOf: fixtureURL)
        )

        XCTAssertEqual(fixture.server.name, "Contract server")
        XCTAssertEqual(fixture.itemDetail.item.title, "The Contract")
        XCTAssertEqual(fixture.page.items?.first?.rollup?.leaves, 20)
        XCTAssertEqual(fixture.decision.delivery?.mode, "remux")
        XCTAssertEqual(fixture.decision.deliveredDynamicRange, "dolby_vision")
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
    func testApplePlayerFailureDetailKeepsTheErrorChainAndHLSVerdict() {
        let underlying = NSError(
            domain: "CoreMediaErrorDomain",
            code: -12927,
            userInfo: [NSLocalizedDescriptionKey: "decoder rejected the stream"]
        )
        let error = NSError(
            domain: "AVFoundationErrorDomain",
            code: -11828,
            userInfo: [NSUnderlyingErrorKey: underlying]
        )
        XCTAssertEqual(
            PlayerController.playbackFailureDetail(
                error: error,
                eventDomain: "CoreMediaErrorDomain",
                eventStatus: -12927,
                eventComment: "Playlist codec is not supported"
            ),
            "error=AVFoundationErrorDomain:-11828 · underlying=CoreMediaErrorDomain:-12927 decoder rejected the stream · event=CoreMediaErrorDomain:-12927 · comment=Playlist codec is not supported"
        )
    }

    @MainActor
    func testApplePlayerNeverTreatsBufferingOrTransportFailureAsCodecFailure() {
        XCTAssertFalse(PlayerController.shouldMonitorSilentPlaybackStall(
            timeControlStatus: .waitingToPlayAtSpecifiedRate
        ))
        XCTAssertTrue(PlayerController.shouldMonitorBufferingStall(
            timeControlStatus: .waitingToPlayAtSpecifiedRate
        ))
        XCTAssertTrue(PlayerController.shouldMonitorSilentPlaybackStall(
            timeControlStatus: .playing
        ))
        XCTAssertFalse(PlayerController.shouldMonitorBufferingStall(
            timeControlStatus: .playing
        ))

        let timeout = NSError(
            domain: NSURLErrorDomain,
            code: NSURLErrorTimedOut,
            userInfo: [NSLocalizedDescriptionKey: "The request timed out."]
        )
        let transportFailure = NSError(
            domain: AVFoundationErrorDomain,
            code: AVError.unknown.rawValue,
            userInfo: [NSUnderlyingErrorKey: timeout]
        )
        XCTAssertFalse(PlayerController.isCompatibilityPlaybackFailure(
            error: transportFailure,
            eventDomain: NSURLErrorDomain,
            eventStatus: NSURLErrorTimedOut,
            eventComment: "segment request timed out"
        ))
        XCTAssertFalse(PlayerController.isCompatibilityPlaybackFailure(
            error: NSError(
                domain: AVFoundationErrorDomain,
                code: AVError.decoderTemporarilyUnavailable.rawValue,
                userInfo: [:]
            ),
            eventDomain: nil,
            eventStatus: nil,
            eventComment: "decoder resources are temporarily unavailable"
        ))
    }

    @MainActor
    func testApplePlayerUsesCompatibilityFallbackOnlyForMediaRejection() {
        let decoderFailure = NSError(
            domain: "CoreMediaErrorDomain",
            code: -12910,
            userInfo: [NSLocalizedDescriptionKey: "video decoder rejected the format"]
        )
        let mediaFailure = NSError(
            domain: AVFoundationErrorDomain,
            code: AVError.fileFormatNotRecognized.rawValue,
            userInfo: [NSUnderlyingErrorKey: decoderFailure]
        )
        XCTAssertTrue(PlayerController.isCompatibilityPlaybackFailure(
            error: mediaFailure,
            eventDomain: "CoreMediaErrorDomain",
            eventStatus: -12910,
            eventComment: "Playlist codec is not supported"
        ))
    }

    @MainActor
    func testApplePlayerFallsBackFromDolbyVisionToHDRBeforeSDR() {
        XCTAssertTrue(PlayerController.hasCompatibleDolbyVisionBase(
            "Dolby Vision · Profile 8 (HDR10-compatible)"
        ))
        XCTAssertTrue(PlayerController.hasCompatibleDolbyVisionBase(
            "Dolby Vision · Profile 8 (HLG-compatible)"
        ))
        XCTAssertFalse(PlayerController.hasCompatibleDolbyVisionBase(
            "Dolby Vision · Profile 5"
        ))

        XCTAssertEqual(
            PlayerController.nextCompatibilityFallback(
                canRetryWithHDRBase: true,
                hdrBaseAlreadyAttempted: false,
                canRetryWithTranscode: true,
                transcodeAlreadyAttempted: false
            ),
            .hdrBase,
            "Profile 8.1 keeps its 10-bit HDR base before any video encode"
        )
        XCTAssertEqual(
            PlayerController.nextCompatibilityFallback(
                canRetryWithHDRBase: false,
                hdrBaseAlreadyAttempted: true,
                canRetryWithTranscode: true,
                transcodeAlreadyAttempted: false
            ),
            .transcode,
            "a failed HDR-base retry may still use the universal fallback"
        )
        XCTAssertEqual(
            PlayerController.nextCompatibilityFallback(
                canRetryWithHDRBase: false,
                hdrBaseAlreadyAttempted: true,
                canRetryWithTranscode: false,
                transcodeAlreadyAttempted: true
            ),
            .none,
            "the recovery ladder cannot loop either compatibility attempt"
        )
    }

    @MainActor
    func testAutomaticSubtitlesApplyTheServersPickInsteadOfRederivingIt() {
        // The Scary Movie shape, as the decision hands it over: the server ran
        // `select_tracks` and stamped `default` on exactly the track it chose,
        // so the muxer's own Italian default is already gone from the wire.
        // The client applies that answer and does not re-derive one.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "ita", title: "Forced",
                default: false, forced: true, text: true
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "ita", title: "Regular",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 2, codec: "subrip", language: "eng", title: "Forced",
                default: true, forced: false, text: true
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

        XCTAssertEqual(PlayerController.automaticSubtitleIndex(tracks), 2)

        // No pick at all — the server's Auto mode under matching audio, or an
        // operator's Off — selects nothing. The client has no fallback rule of
        // its own to reach past that with.
        let unpicked = tracks.map { track -> SubtitleTrack in
            var copy = track
            copy.default = false
            return copy
        }
        XCTAssertNil(PlayerController.automaticSubtitleIndex(unpicked))

        // Anything the server can choose is applied, including a track in
        // another language or with no language tag at all: those are its
        // rules — dual-audio anime, untagged eligibility, the subtitle mode —
        // and this client no longer keeps a second copy of them.
        var untagged = unpicked
        untagged[1] = SubtitleTrack(
            index: 1, codec: "subrip", language: nil, title: nil,
            default: true, forced: false, text: true
        )
        XCTAssertEqual(PlayerController.automaticSubtitleIndex(untagged), 1)
    }

    /// Pins every row of the standing subtitle rule: automatic selection never
    /// starts a burn, except a forced track, which may. Rows that return nil
    /// are the point — an encoder slot per play, H.264 SDR, HDR gone, is what
    /// the deleted `?? matching.first` tail used to buy.
    @MainActor
    func testAutomaticSubtitlePolicyPinsAllFourRowsOfTheSubtitleRule() {
        // Row 1 — a forced track selected by the server (the `default` wire
        // flag is its pick), whatever the codec. A forced PGS is the one burn
        // automatic selection is allowed to start, because a film whose foreign
        // dialogue is unsubtitled is not watchable at all.
        let forcedBitmap = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Forced",
                default: true, forced: true, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "subrip", language: "eng", title: "Regular",
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(forcedBitmap),
            0,
            "forced-PGS is the one permitted automatic burn"
        )
        // Both forced signals reach the same carve-out: some muxes set only the
        // title.
        let titleOnlyForcedBitmap = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Forced Narrative",
                default: true, forced: false, text: false
            ),
        ]
        XCTAssertTrue(PlayerController.isForcedSubtitle(titleOnlyForcedBitmap[0]))
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex(titleOnlyForcedBitmap),
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
            PlayerController.automaticSubtitleIndex(defaultText),
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
            PlayerController.automaticSubtitleIndex(defaultBitmapOnly),
            "a default-flagged bitmap track must never cold-start a burn transcode"
        )
        let defaultStyled = [
            SubtitleTrack(
                index: 0, codec: "ass", language: "eng", title: "Signs",
                default: true, forced: false, text: false
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(defaultStyled),
            "styled ASS cannot become a WebVTT rendition, so it burns — never automatically"
        )

        // Row 4 — merely the same language, under audio that already speaks it.
        // This is the deleted tail: an unflagged English track no longer
        // captions every English film, and an unflagged English PGS no longer
        // transcodes one. The audio language is what the real call site passes
        // (`audioLanguage: chosenAudio?.language`), and it is the server's own
        // `SubMode::Auto` rule: only the floor stays eligible when the audio
        // already speaks the preferred subtitle language. Japanese audio with
        // an unflagged English SRT is the opposite case and does get subtitles
        // — see `testAutomaticSubtitlesHonorTheServersAutoSubtitleMode`.
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
            PlayerController.automaticSubtitleIndex(unflaggedText)
        )
        let unflaggedBitmapOnly = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: false, forced: false, text: false
            ),
        ]
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(unflaggedBitmapOnly),
            "unflagged English PGS only: cold start must attach no encoder"
        )

        // Language policy belongs to the server. If it marks this Italian
        // native track as the selection, the client applies that answer.
        let italianDefault = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "ita", title: "Italiano",
                default: true, forced: false, text: true
            ),
        ]
        XCTAssertEqual(PlayerController.automaticSubtitleIndex(italianDefault), 0)
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

    func testRelativeSeeksAccumulateFromTheLatestTargetAndClampToTheFilm() {
        var state = PlayerSeekState()

        let first = state.relative(by: 10_000, observedMs: 60_000, durationMs: 120_000)
        let second = state.relative(by: 10_000, observedMs: 60_200, durationMs: 120_000)
        let third = state.relative(by: -10_000, observedMs: 60_400, durationMs: 120_000)

        XCTAssertEqual(first.target, 70_000)
        XCTAssertEqual(second.target, 80_000, "the old AVPlayer clock cannot swallow a second press")
        XCTAssertEqual(third.target, 70_000, "back steps from the optimistic target too")
        XCTAssertEqual(state.pendingMs, 70_000)

        XCTAssertFalse(
            state.complete(generation: first.generation),
            "a cancelled native seek cannot clear a newer target"
        )
        XCTAssertEqual(state.pendingMs, 70_000)
        XCTAssertTrue(state.complete(generation: third.generation))
        XCTAssertNil(state.pendingMs)

        XCTAssertEqual(
            state.absolute(-50_000, durationMs: 120_000).target,
            0
        )
        XCTAssertEqual(
            state.absolute(999_000, durationMs: 120_000).target,
            118_000
        )
    }

    func testDirectionalSeeksUseShortHorizontalAndLongVerticalSteps() {
        XCTAssertEqual(PlayerSeekDirection.left.seconds, -10)
        XCTAssertEqual(PlayerSeekDirection.right.seconds, 10)
        XCTAssertEqual(PlayerSeekDirection.down.seconds, -30)
        XCTAssertEqual(PlayerSeekDirection.up.seconds, 30)
    }

    func testLiveSeekTargetClearsOnlyAfterTheAttachedReopen() {
        var state = PlayerSeekState()
        _ = state.absolute(90_000, durationMs: 600_000)

        state.completeReopen(at: 60_000)
        XCTAssertEqual(state.pendingMs, 90_000)

        state.completeReopen(at: 90_000)
        XCTAssertNil(state.pendingMs)
    }

    func testStallDetectorIgnoresPausesAndRecoversOnlyAfterSustainedNoProgress() {
        var detector = PlaybackStallDetector()

        XCTAssertEqual(detector.sample(positionMs: 10_000, shouldMonitor: false), .none)
        XCTAssertEqual(detector.sample(positionMs: 10_000, shouldMonitor: true), .none)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .none)

        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .none)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .none)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .nudge)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .none)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .none)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .reopen)

        // A deliberate pause resets the wall-clock evidence rather than
        // letting it carry into the next press of Play.
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: false), .none)
        XCTAssertEqual(detector.sample(positionMs: 12_000, shouldMonitor: true), .none)
    }

    @MainActor
    func testBufferingWaitHasAnIndependentBoundedRecoveryTimer() {
        var monitor = PlaybackRecoveryMonitor()
        let beganAt: TimeInterval = 1_000

        for check in 0...6 {
            let event = monitor.sample(
                positionMs: 30_000,
                timeControlStatus: .waitingToPlayAtSpecifiedRate,
                shouldMonitor: true,
                establishedPlayback: true,
                observedAt: beganAt + Double(check * 2)
            )
            if check == 3 {
                XCTAssertEqual(
                    event,
                    PlaybackStallEvent(
                        kind: .buffering,
                        action: .nudge,
                        positionMs: 30_000,
                        durationMs: 6_000
                    )
                )
            } else if check == 6 {
                XCTAssertEqual(
                    event,
                    PlaybackStallEvent(
                        kind: .buffering,
                        action: .reopen,
                        positionMs: 30_000,
                        durationMs: 12_000
                    )
                )
            } else {
                XCTAssertNil(event)
            }
        }

        XCTAssertNil(monitor.silentDetector.lastPositionMs)
        XCTAssertEqual(monitor.bufferingDetector.lastPositionMs, 30_000)
    }

    @MainActor
    func testBufferingRecoveryCannotStartBeforeTheFirstFiveSecondsPlay() {
        var monitor = PlaybackRecoveryMonitor()
        let beganAt: TimeInterval = 2_000

        for check in 0...10 {
            XCTAssertNil(monitor.sample(
                positionMs: 0,
                timeControlStatus: .waitingToPlayAtSpecifiedRate,
                shouldMonitor: true,
                establishedPlayback: false,
                observedAt: beganAt + Double(check * 2)
            ))
        }
        XCTAssertNil(monitor.bufferingDetector.lastPositionMs)

        for check in 0...6 {
            let event = monitor.sample(
                positionMs: 30_000,
                timeControlStatus: .waitingToPlayAtSpecifiedRate,
                shouldMonitor: true,
                establishedPlayback: true,
                observedAt: beganAt + Double(30 + check * 2)
            )
            if check == 6 {
                XCTAssertEqual(event?.kind, .buffering)
                XCTAssertEqual(event?.action, .reopen)
            }
        }
    }

    @MainActor
    func testEveryOpenedItemMustEstablishItsOwnBufferingRecoveryWindow() {
        var attachment = PlayerAttachmentRecoveryState()
        attachment.opened(at: 10_000)
        attachment.observe(positionMs: 14_999, playing: true)
        XCTAssertFalse(attachment.establishedPlayback)
        attachment.observe(positionMs: 15_000, playing: true)
        XCTAssertTrue(attachment.establishedPlayback)

        attachment.opened(at: 90_000)
        XCTAssertFalse(
            attachment.establishedPlayback,
            "a quality/audio/recovery open must not inherit the predecessor's gate"
        )

        var monitor = PlaybackRecoveryMonitor()
        for check in 0...7 {
            XCTAssertNil(monitor.sample(
                positionMs: 90_000,
                timeControlStatus: .waitingToPlayAtSpecifiedRate,
                shouldMonitor: true,
                establishedPlayback: attachment.establishedPlayback,
                observedAt: 3_000 + Double(check * 2)
            ))
        }
        XCTAssertNil(monitor.bufferingDetector.lastPositionMs)

        attachment.observe(positionMs: 95_000, playing: true)
        XCTAssertTrue(attachment.establishedPlayback)
    }

    @MainActor
    func testSubthresholdStallIsHeldForTheNextProgressReport() {
        var monitor = PlaybackRecoveryMonitor()
        let beganAt: TimeInterval = 4_000

        for check in 0...4 {
            XCTAssertNotEqual(monitor.sample(
                positionMs: 30_000,
                timeControlStatus: .waitingToPlayAtSpecifiedRate,
                shouldMonitor: true,
                establishedPlayback: true,
                observedAt: beganAt + Double(check * 2)
            )?.action, .reopen)
        }
        XCTAssertNil(monitor.sample(
            positionMs: 31_000,
            timeControlStatus: .playing,
            shouldMonitor: true,
            establishedPlayback: true,
            observedAt: beganAt + 10
        ))

        var observations = PlaybackStallObservationState()
        observations.noteRecoveredStagnation(monitor.takeRecoveredStagnantDurationMs())
        XCTAssertEqual(
            observations.take(numberOfStalls: 1),
            PlaybackStallObservation(delta: 1, stagnantDurationMs: 10_000)
        )
        XCTAssertNil(observations.take(numberOfStalls: 1))
        XCTAssertEqual(
            observations.take(numberOfStalls: 3),
            PlaybackStallObservation(delta: 2, stagnantDurationMs: 0)
        )
    }

    @MainActor
    func testPlaybackRecoveryPredicatesStayExclusiveAndBufferingWinsAMerge() {
        for status in [
            AVPlayer.TimeControlStatus.paused,
            .waitingToPlayAtSpecifiedRate,
            .playing,
        ] {
            XCTAssertNotEqual(
                PlayerController.shouldMonitorSilentPlaybackStall(timeControlStatus: status),
                PlayerController.shouldMonitorBufferingStall(timeControlStatus: status)
            )
        }

        XCTAssertEqual(
            PlaybackRecoveryMonitor.select(
                silentAction: .reopen,
                bufferingAction: .nudge
            ),
            PlaybackStallSelection(kind: .buffering, action: .nudge)
        )
    }

    func testRepeatedBufferingRecoveryStopsWithNetworkSpecificState() {
        var recovery = SameDeliveryStallRecoveryState()

        XCTAssertEqual(recovery.next(for: .buffering), .reopen)
        let repeated = recovery.next(for: .buffering)
        XCTAssertEqual(repeated, .stop(PlaybackStallKind.buffering.terminalState))

        let terminal = PlaybackStallKind.buffering.terminalState
        XCTAssertFalse(terminal.isPlaying)
        XCTAssertFalse(terminal.wantsPlayback)
        XCTAssertTrue(terminal.failed)
        XCTAssertEqual(
            terminal.message,
            "Playback could not resume after repeated buffering. Check the connection and try again."
        )
        XCTAssertNotEqual(terminal.message, PlaybackStallKind.silent.terminalState.message)
    }

    func testAppleBufferingStallLogCarriesPositionMethodAndDuration() throws {
        let payload = ApplePlaybackStallLog(
            kind: .buffering,
            outcome: .reopen,
            positionMs: 90_000,
            durationMs: 12_000,
            method: "remux",
            title: "Fortune Feimster",
            fileId: 42,
            vcodec: "h264",
            encoder: "copy"
        )
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(payload))
                as? [String: Any]
        )

        XCTAssertEqual(object["event"] as? String, "stall")
        XCTAssertEqual(object["method"] as? String, "remux")
        XCTAssertEqual(object["ms"] as? Int, 12_000)
        XCTAssertEqual(object["file_id"] as? Int, 42)
        XCTAssertEqual(object["encoder"] as? String, "copy")
        XCTAssertEqual(
            object["detail"] as? String,
            "kind=buffering · position_ms=90000 · outcome=reopen"
        )
    }

    func testAppleObservedStallLogCarriesDeltaWithoutCapabilityData() throws {
        let payload = ApplePlaybackObservedStallLog(
            delta: 2,
            positionMs: 90_000,
            stagnantDurationMs: 8_000,
            method: "remux",
            title: "Fortune Feimster",
            fileId: 42,
            vcodec: "h264",
            encoder: "copy"
        )
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(payload))
                as? [String: Any]
        )

        XCTAssertEqual(object["event"] as? String, "stall")
        XCTAssertEqual(object["ms"] as? Int, 8_000)
        XCTAssertEqual(
            object["detail"] as? String,
            "kind=access_log · position_ms=90000 · stall_delta=2 · outcome=self_recovered"
        )
        XCTAssertNil(object["url"])
        XCTAssertNil(object["token"])
    }

    @MainActor
    func testGrowingHLSSessionStaysInsideTheServerRetentionContract() {
        let item = AVPlayerItem(url: URL(fileURLWithPath: "/dev/null"))

        PlayerController.configureBuffering(item, growingHLS: true)
        XCTAssertEqual(
            item.preferredForwardBufferDuration,
            PlayerController.growingHLSForwardBufferSeconds
        )

        PlayerController.configureBuffering(item, growingHLS: false)
        XCTAssertEqual(
            item.preferredForwardBufferDuration,
            0,
            "direct and completed-VOD items keep AVPlayer's normal buffering policy"
        )
    }

    @MainActor
    func testAutomaticSubtitlesVetoANonForcedBitmapPick() {
        // A Blu-ray remux whose English subtitle is a non-forced PGS. Honoring
        // that pick would spawn a video encoder on every single play — the
        // exact bug this project exists to kill — so the client vetoes it and
        // leaves the viewer to ask for it by hand.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: true, forced: false, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "dvd_subtitle", language: "eng", title: "Commentary",
                default: false, forced: false, text: false
            ),
        ]

        XCTAssertNil(PlayerController.automaticSubtitleIndex(tracks))
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(0, in: tracks))
    }

    @MainActor
    func testAutomaticSubtitlesVetoAStyledAssPick() {
        // An anime MKV whose picked track is ASS: styled subtitles stay burns
        // because WebVTT cannot carry their authored presentation, so the veto
        // covers them too even though they are text.
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

        XCTAssertNil(PlayerController.automaticSubtitleIndex(tracks))
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
                default: true, forced: true, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: false, forced: false, text: false
            ),
        ]

        XCTAssertEqual(PlayerController.automaticSubtitleIndex(tracks), 0)
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
    func testHDRSubtitleGuardKeepsTheCurrentPictureForBurnOnlyTracks() {
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "English",
                default: false, forced: false, text: false
            ),
            SubtitleTrack(
                index: 1, codec: "ass", language: "jpn", title: "Signs",
                default: false, forced: false, text: true
            ),
            SubtitleTrack(
                index: 2, codec: "subrip", language: "eng", title: "English text",
                default: false, forced: false, text: true
            ),
        ]

        for range in ["dolby_vision", "hdr10", "hlg"] {
            XCTAssertTrue(PlayerController.subtitleBurnWouldDiscardHDR(
                0, tracks: tracks, deliveredRange: range
            ))
            XCTAssertTrue(PlayerController.subtitleBurnWouldDiscardHDR(
                1, tracks: tracks, deliveredRange: range
            ))
            XCTAssertFalse(PlayerController.subtitleBurnWouldDiscardHDR(
                2, tracks: tracks, deliveredRange: range
            ))
        }
        XCTAssertFalse(PlayerController.subtitleBurnWouldDiscardHDR(
            0, tracks: tracks, deliveredRange: "sdr"
        ))
        XCTAssertFalse(PlayerController.subtitleBurnWouldDiscardHDR(
            nil, tracks: tracks, deliveredRange: "hdr10"
        ))
    }

    @MainActor
    func testNonfatalPlaybackNoticeExpiresWithoutBecomingAPlaybackError() async {
        let controller = PlayerController()

        controller.showPlaybackNotice(
            PlayerController.hdrSubtitleNotice,
            duration: .milliseconds(10)
        )

        XCTAssertEqual(controller.playbackNotice, PlayerController.hdrSubtitleNotice)
        XCTAssertNil(controller.playbackError)
        XCTAssertFalse(controller.failed)

        try? await Task.sleep(for: .milliseconds(50))

        XCTAssertNil(controller.playbackNotice)
        XCTAssertNil(controller.playbackError)
        XCTAssertFalse(controller.failed)
    }

    @MainActor
    func testEstablishedHDRRecoveryNeverChoosesTheSDRFallback() {
        XCTAssertTrue(PlayerController.shouldPreserveEstablishedHDRDelivery(
            deliveredRange: "hdr10", establishedPlayback: true
        ))
        XCTAssertTrue(PlayerController.shouldPreserveEstablishedHDRDelivery(
            deliveredRange: "dolby_vision", establishedPlayback: true
        ))
        XCTAssertFalse(PlayerController.shouldPreserveEstablishedHDRDelivery(
            deliveredRange: "hdr10", establishedPlayback: false
        ))
        XCTAssertFalse(PlayerController.shouldPreserveEstablishedHDRDelivery(
            deliveredRange: "sdr", establishedPlayback: true
        ))
    }

    @MainActor
    func testNativeClassificationComesFromTheServerWithACodecFallback() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        func track(_ json: String) throws -> SubtitleTrack {
            try decoder.decode(SubtitleTrack.self, from: Data(json.utf8))
        }

        // The server answers directly, and its answer wins — `text` is not the
        // same question, which is what has bitten every client that assumed it
        // was: ASS carries text and is still a burn.
        let styled = try track(
            #"{"index":0,"codec":"ass","default":false,"forced":false,"text":true,"native":false}"#
        )
        XCTAssertFalse(styled.isNativeHLS)
        XCTAssertTrue(styled.text)
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(0, in: [styled]))

        // A format this client's own list does not know is still native when
        // the server says so — one classifier, so the client cannot ask for a
        // session the server would refuse.
        let futureText = try track(
            #"{"index":1,"codec":"mov_text","default":false,"forced":false,"text":true,"native":true}"#
        )
        XCTAssertTrue(futureText.isNativeHLS)

        // A server predating the field degrades exactly as it does today.
        let legacy = try track(
            #"{"index":2,"codec":"subrip","default":false,"forced":false,"text":true}"#
        )
        XCTAssertNil(legacy.native)
        XCTAssertTrue(legacy.isNativeHLS)
        let legacyBitmap = try track(
            #"{"index":3,"codec":"hdmv_pgs_subtitle","default":false,"forced":false,"text":false}"#
        )
        XCTAssertFalse(legacyBitmap.isNativeHLS)

        // And the veto reads the same answer: a server-picked track the server
        // itself classifies burn-only is not auto-selected.
        var picked = styled
        picked.default = true
        XCTAssertNil(PlayerController.automaticSubtitleIndex([picked]))
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

        // The alias table also decides which advertised rendition a track
        // resolves onto, so a "dut"-tagged track has to match a "nl" option.
        let dutch = [
            SubtitleTrack(
                index: 0, codec: "subrip", language: "dut", title: nil,
                default: false, forced: false, text: true
            ),
        ]
        XCTAssertEqual(
            PlayerController.nativeSubtitleOptionIndex(
                ordinal: 0,
                tracks: dutch,
                options: [
                    SubtitleRenditionOption(languageTag: "en", displayName: "English"),
                    SubtitleRenditionOption(languageTag: "nl", displayName: "Dutch"),
                ]
            ),
            1
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
        // A forced track is the veto's one exception, so it is the only path
        // by which automatic selection may start a burn: an over-eager title
        // test hands that exception to ordinary tracks.
        XCTAssertTrue(PlayerController.titleMarksForced("Forced"))
        XCTAssertTrue(PlayerController.titleMarksForced("English Forced"))
        XCTAssertTrue(PlayerController.titleMarksForced("forced (signs)"))
        XCTAssertFalse(PlayerController.titleMarksForced("Non-Forced"))
        XCTAssertFalse(PlayerController.titleMarksForced("non forced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Not Forced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Unforced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Reinforced"))
        XCTAssertFalse(PlayerController.titleMarksForced("Full"))

        // The server picked this one, and the veto still refuses it: a title
        // that only mentions being *not* forced buys no exception.
        let tracks = [
            SubtitleTrack(
                index: 0, codec: "hdmv_pgs_subtitle", language: "eng", title: "Non-Forced",
                default: true, forced: false, text: false
            ),
        ]
        XCTAssertFalse(PlayerController.isForcedSubtitle(tracks[0]))
        XCTAssertNil(
            PlayerController.automaticSubtitleIndex(tracks),
            "a \"Non-Forced\" PGS track must not auto-burn on every play"
        )

        var flagged = tracks[0]
        flagged.forced = true
        XCTAssertTrue(
            PlayerController.isForcedSubtitle(flagged),
            "the container disposition still stands on its own"
        )
        XCTAssertEqual(
            PlayerController.automaticSubtitleIndex([flagged]),
            0,
            "a genuinely forced pick keeps the burn carve-out"
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

    /// Both halves of the Settings toggle, which is the only thing that reads
    /// `SubtitleReadiness`. `.onDemand` is the shipped default; `.instant` must
    /// keep answering exactly as the old `!hasNativeSubtitles` guard did.
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
    /// must both direct-play first: `.onDemand` is the default Paul chose on
    /// 2026-08-02, so a play that never opens the subtitle menu never asks the
    /// server for a session.
    func testSubtitleReadinessDefaultsToDirectPlayFirstAndPersistsAChange() throws {
        let suite = "tv.plurx.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let fresh = SettingsStore(defaults: defaults)
        XCTAssertEqual(fresh.subtitleReadiness, .onDemand)

        // An unreadable or future value falls back to the default rather than
        // silently changing how titles open.
        defaults.set("something-else", forKey: "plurx.subtitleReadiness")
        XCTAssertEqual(SettingsStore(defaults: defaults).subtitleReadiness, .onDemand)

        fresh.subtitleReadiness = .instant
        XCTAssertEqual(SettingsStore(defaults: defaults).subtitleReadiness, .instant)
    }

    func testNativeAppearanceSettingsPreserveTheExistingLookAndPersistChoices() throws {
        let suite = "tv.plurx.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let fresh = SettingsStore(defaults: defaults)
        XCTAssertEqual(fresh.theme, .noirr)
        XCTAssertEqual(fresh.appearance, .dark)

        defaults.set("future-theme", forKey: "plurx.theme")
        defaults.set("future-appearance", forKey: "plurx.appearance")
        XCTAssertEqual(SettingsStore(defaults: defaults).theme, .noirr)
        XCTAssertEqual(SettingsStore(defaults: defaults).appearance, .dark)

        fresh.theme = .terminal
        fresh.appearance = .light
        let restored = SettingsStore(defaults: defaults)
        XCTAssertEqual(restored.theme, .terminal)
        XCTAssertEqual(restored.appearance, .light)
    }

    func testNativeAppearanceChoicesMatchTheOtherNativeViewer() {
        XCTAssertEqual(ViewerTheme.allCases.map(\.label), ["Classic", "Terminal", "noirr"])
        XCTAssertEqual(
            ViewerAppearance.allCases.map(\.label),
            ["Auto (system)", "Light", "Dark"]
        )
        XCTAssertNil(ViewerAppearance.system.preferredColorScheme)
        XCTAssertEqual(ViewerAppearance.light.preferredColorScheme, .light)
        XCTAssertEqual(ViewerAppearance.dark.preferredColorScheme, .dark)
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
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 0, tracks: tracks, activeBurn: nil, isDirectPlayback: true
            ),
            .reopen
        )
        // Turning subtitles off during direct play restarts nothing — there was
        // never anything on.
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: nil, tracks: tracks, activeBurn: nil, isDirectPlayback: true
            ),
            .mediaSelection
        )
        // Once the copy session exists, every further text switch is free
        // again: this is the second-and-later selection, and it must not
        // reopen.
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 0, tracks: tracks, activeBurn: nil, isDirectPlayback: false
            ),
            .mediaSelection
        )

        // The pre-existing reasons are untouched: entering a burn, and leaving
        // one, still reopen whatever the delivery mode.
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 1, tracks: tracks, activeBurn: nil, isDirectPlayback: false
            ),
            .reopen
        )
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: nil, tracks: tracks, activeBurn: 1, isDirectPlayback: false
            ),
            .reopen
        )
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 0, tracks: tracks, activeBurn: 1, isDirectPlayback: false
            ),
            .reopen
        )
        // An index the decision never listed is treated as a burn, as before.
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 99, tracks: tracks, activeBurn: nil, isDirectPlayback: false
            ),
            .reopen
        )
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
        // whole rule, not a failure. English audio, so the server's Auto mode
        // leaves only the floor eligible and the unflagged native SDH track is
        // not it.
        XCTAssertNil(PlayerController.automaticSubtitleIndex(tracks))

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
        XCTAssertEqual(PlayerController.automaticSubtitleIndex(flagged), 2)

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
            PlayerController.automaticSubtitleIndex(forcedMovText),
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

    func testAppleCapsKeepGenericHDRSeparateFromDolbyVision() {
        func dictionary(_ query: [URLQueryItem]) -> [String: String] {
            Dictionary(uniqueKeysWithValues: query.compactMap { item in
                item.value.map { (item.name, $0) }
            })
        }

        // This is the tvOS failure that used to become an SDR compatibility
        // transcode: HDR10 output made the client claim Dolby Vision too.
        let hdrOnly = dictionary(Caps.query(
            hevc: true,
            av1: false,
            displayHDR: true,
            dolbyVision: false
        ))
        XCTAssertEqual(hdrOnly["hdr"], "1")
        XCTAssertEqual(hdrOnly["dv"], "0")
        XCTAssertEqual(hdrOnly["dvprofile"], "")
        XCTAssertEqual(hdrOnly["dvhls"], "1")
        XCTAssertEqual(hdrOnly["vcodec"], "h264,hevc")

        let dolbyVision = dictionary(Caps.query(
            hevc: true,
            av1: true,
            displayHDR: true,
            dolbyVision: true
        ))
        XCTAssertEqual(dolbyVision["hdr"], "1")
        XCTAssertEqual(dolbyVision["dv"], "1")
        XCTAssertEqual(dolbyVision["dvprofile"], "5,8")
        XCTAssertEqual(dolbyVision["dvhls"], "1")
        XCTAssertEqual(dolbyVision["vcodec"], "h264,hevc,av1")

        // A DV-capable display without the required hardware decoder is not a
        // playable DV path and must not be advertised as one.
        let noHEVC = dictionary(Caps.query(
            hevc: false,
            av1: false,
            displayHDR: true,
            dolbyVision: true
        ))
        XCTAssertEqual(noHEVC["hdr"], "1")
        XCTAssertEqual(noHEVC["dv"], "0")
        XCTAssertEqual(noHEVC["dvprofile"], "")

        // The runtime path still spells every field for old and new servers.
        let runtime = Dictionary(uniqueKeysWithValues: Caps.query().compactMap { item in
            item.value.map { (item.name, $0) }
        })
        print("PLURX_CAPABILITIES \(runtime)")
        XCTAssertNotNil(runtime["hdr"])
        XCTAssertNotNil(runtime["dv"])
        XCTAssertNotNil(runtime["dvprofile"])
        XCTAssertEqual(runtime["client"], "apple")
        XCTAssertNotEqual(runtime["device"], "")
        if runtime["dv"] == "1" {
            XCTAssertEqual(runtime["hdr"], "1")
            XCTAssertEqual(runtime["dvprofile"], "5,8")
            XCTAssertTrue(runtime["vcodec"]?.split(separator: ",").contains("hevc") == true)
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
            ["2160p", "Dolby Vision · Profile 8", "Dolby Digital Plus 5.1"]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.mark),
            ["2160P", "DV P8", "DD+ 5.1"]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.kind),
            [.resolution, .dynamicRange, .audio]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.tone),
            [.resolution, .dolbyVision, .audio]
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
            ["2160p"]
        )

        XCTAssertEqual(PlayerView.playbackResolutionLabel(width: nil, height: 1_440), "1440p")
        XCTAssertEqual(PlayerView.playbackResolutionLabel(width: 720, height: 480), "480p")
        XCTAssertNil(PlayerView.playbackResolutionLabel(width: nil, height: nil))

        var mono = audio
        mono.channels = 1
        XCTAssertEqual(PlayerView.soundLabel(mono)?.mark, "DD+ MONO")
        XCTAssertEqual(
            PlayerView.soundLabel(mono)?.accessibilityLabel,
            "Dolby Digital Plus Mono"
        )
        var sixPointOne = audio
        sixPointOne.channels = 7
        XCTAssertEqual(PlayerView.soundLabel(sixPointOne)?.mark, "DD+ 6.1")
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
        // the rich source chip the web player draws.
        let sourceOnly = try badge(nil)
        XCTAssertEqual(sourceOnly.mark, "DV P8")
        XCTAssertEqual(sourceOnly.tone, .dolbyVision)
        XCTAssertFalse(sourceOnly.dimmed)

        // Lit: the copy session kept the RPUs and the display can show them.
        let lit = try badge("dolby_vision")
        XCTAssertEqual(lit.mark, "DV P8")
        XCTAssertEqual(
            lit.accessibilityLabel,
            "Dolby Vision · Profile 8 (HDR10-compatible)"
        )
        XCTAssertFalse(lit.dimmed)

        // Downgraded by the server: an unclaimed profile takes the strip path,
        // which delivers the compatible HDR10 base.
        let stripped = try badge("hdr10")
        XCTAssertEqual(stripped.mark, "DV P8")
        XCTAssertEqual(stripped.renderedMark, "HDR10")
        XCTAssertEqual(stripped.displayMark, "DV P8 → HDR10")
        XCTAssertEqual(stripped.accessibilityLabel, "Dolby Vision, playing as HDR10")
        XCTAssertTrue(stripped.dimmed, "only the unavailable DV half is subdued")

        // Downgraded by the transcode: a burn or a picked rung is H.264 8-bit.
        let transcoded = try badge("sdr")
        XCTAssertEqual(transcoded.displayMark, "DV P8 → SDR")
        XCTAssertTrue(transcoded.dimmed)

        // Downgraded by the display: delivered bits are necessary, not
        // sufficient. This is the whole of what the client is allowed to know
        // about rendering — no headroom polling, no variant introspection.
        let sdrPanel = try badge("dolby_vision", displayHDR: false)
        XCTAssertEqual(sdrPanel.displayMark, "DV P8 → SDR")
        XCTAssertTrue(sdrPanel.dimmed)

        // A plain HDR10 source keeps the terse base mark and reports its own
        // losses; an SDR source has no grade to report on at all.
        let hdr10 = try XCTUnwrap(PlayerView.dynamicRangeBadge(
            hdr: "hdr10", hdrFormat: "HDR10", delivered: "sdr", displayHDR: true
        ))
        XCTAssertEqual(hdr10.displayMark, "HDR10 → SDR")
        XCTAssertEqual(hdr10.accessibilityLabel, "HDR10, playing as SDR")
        XCTAssertEqual(hdr10.tone, .hdr)
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
            ["2160P", "DV P7", "ATMOS 7.1"]
        )
        let downgraded = PlayerView.playbackBadges(
            source: source, audio: audio, delivered: "sdr", displayHDR: true
        )
        XCTAssertEqual(
            downgraded.map(\.displayMark),
            ["2160P", "DV P7 → SDR", "ATMOS 7.1"]
        )
        XCTAssertEqual(downgraded.map(\.dimmed), [false, true, false])
        XCTAssertEqual(
            downgraded.map(\.tone),
            [.resolution, .dolbyVision, .audio]
        )
        XCTAssertLessThanOrEqual(PlayerMetadataBadgeMetrics.dimmedOpacity, 0.5)
        XCTAssertGreaterThan(PlayerMetadataBadgeMetrics.dimmedOpacity, 0.2)
    }

    func testInitialHlsSessionCarriesTheDecisionsAudioTrack() {
        XCTAssertEqual(
            PlayerController.sessionAudioIndex(explicit: nil, selected: 2),
            2,
            "cold start must carry the track whose checkmark the decision supplied"
        )
        XCTAssertEqual(
            PlayerController.sessionAudioIndex(explicit: 3, selected: 2),
            3,
            "a viewer's manual selection must still win on a reopen"
        )
        XCTAssertNil(PlayerController.sessionAudioIndex(explicit: nil, selected: nil))
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

    func testHlsMediaOriginPrefersTheActualKeyframeAndFallsBackCompatibly() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase

        let copied = try decoder.decode(HlsStart.self, from: Data(#"""
        {"session_id":"copy","playlist_url":"/hls/copy/index.m3u8",
         "start_seconds":10.5,"media_origin_ms":10000,"vod":false}
        """#.utf8))
        XCTAssertEqual(copied.mediaOriginMs, 10_000)
        XCTAssertEqual(
            PlayerController.sessionMediaOriginMs(copied, requestedStartMs: 10_500),
            10_000
        )

        let legacy = try decoder.decode(HlsStart.self, from: Data(#"""
        {"session_id":"legacy","playlist_url":"/hls/legacy/index.m3u8",
         "start_seconds":10.5,"vod":false}
        """#.utf8))
        XCTAssertNil(legacy.mediaOriginMs)
        XCTAssertEqual(
            PlayerController.sessionMediaOriginMs(legacy, requestedStartMs: 10_500),
            10_500
        )

        let cached = try decoder.decode(HlsStart.self, from: Data(#"""
        {"session_id":"cached","playlist_url":"/hls/cached/index.m3u8",
         "start_seconds":0,"media_origin_ms":123000,"vod":true}
        """#.utf8))
        XCTAssertEqual(
            PlayerController.sessionMediaOriginMs(cached, requestedStartMs: 123_000),
            0
        )
    }

    /// Servers from before the Apple DV transport hint can approve Profile 8
    /// as progressive direct play. AVPlayer then advances with audio while the
    /// video plane stays black. The client must losslessly repackage that same
    /// video as HLS and retain its DV metadata.
    func testLegacyDirectDolbyVisionIsNormalizedToPreservingHls() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let decision = try decoder.decode(Decision.self, from: Data(#"""
        {"file_id":5657,"method":"direct_play",
         "play_url":"/api/v1/files/5657/direct",
         "source":{"container":"mp4","video_codec":"hevc",
                   "hdr":"dolby_vision",
                   "hdr_format":"Dolby Vision · Profile 8 (HDR10-compatible)"}}
        """#.utf8))

        XCTAssertEqual(PlayerController.playbackMode(decision), "remux")
        XCTAssertTrue(PlayerController.shouldPreserveDolbyVision(decision))
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
        XCTAssertEqual(range.mark, "DV P8")
        XCTAssertEqual(
            range.accessibilityLabel,
            "Dolby Vision · Profile 8 (HDR10-compatible)"
        )

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
            changingStream: false,
            optionMenuOpen: false
        ))
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: false,
            changingStream: false,
            optionMenuOpen: true
        ))
        XCTAssertTrue(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: false,
            changingStream: false,
            optionMenuOpen: false
        ))
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: false,
            scrubbing: false,
            changingStream: false,
            optionMenuOpen: false
        ))
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: false,
            changingStream: false,
            optionMenuOpen: false,
            tearingDown: true
        ))
    }

    func testPlaybackInfoStaysVisibleAfterControlsAutoHide() {
        let visible = PlayerView.overlayVisibility(
            controlsVisible: false,
            playbackInfoVisible: true
        )

        XCTAssertFalse(visible.controls)
        XCTAssertTrue(visible.playbackInfo)
    }

    func testNaturalPlaybackEndDismissesUnlessOnlineAutoplayCanLookForNext() {
        XCTAssertNil(PlayerView.naturalEndAction(
            finished: false,
            autoplay: false,
            offline: false
        ))
        XCTAssertEqual(PlayerView.naturalEndAction(
            finished: true,
            autoplay: false,
            offline: false
        ), .dismiss)
        XCTAssertEqual(PlayerView.naturalEndAction(
            finished: true,
            autoplay: true,
            offline: true
        ), .dismiss)
        XCTAssertEqual(PlayerView.naturalEndAction(
            finished: true,
            autoplay: true,
            offline: false
        ), .findNext)
    }

    func testUnknownDurationEndReopensUntilAnEndIsCorroborated() {
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: nil,
            isGrowingPlaylist: true,
            endedAt: 120_000
        ), .reopen)
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 180_000,
            itemDurationMs: nil,
            isGrowingPlaylist: true,
            endedAt: 120_000
        ), .reopen)
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: 180_000,
            isGrowingPlaylist: true,
            endedAt: 178_000
        ), .reopen)
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: 180_000,
            isGrowingPlaylist: false,
            endedAt: 178_000
        ), .finish(durationMs: 180_000))
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 180_000,
            itemDurationMs: nil,
            isGrowingPlaylist: true,
            endedAt: 178_000
        ), .finish(durationMs: 180_000))
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: nil,
            isGrowingPlaylist: false,
            endedAt: 120_000
        ), .reopen)
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: nil,
            isGrowingPlaylist: false,
            endedAt: 120_000,
            previousUncorroboratedEndMs: 120_000
        ), .finish(durationMs: 120_000))
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: nil,
            isGrowingPlaylist: true,
            endedAt: 120_000,
            previousUncorroboratedEndMs: 120_000
        ), .reopen, "a genuinely growing playlist may publish more media")
        XCTAssertEqual(PlayerController.endAction(
            knownDurationMs: 0,
            itemDurationMs: nil,
            isGrowingPlaylist: false,
            endedAt: 121_000,
            previousUncorroboratedEndMs: 120_000
        ), .reopen, "positional progress earns a fresh bounded retry")
    }

    @MainActor
    func testNaturalEndActionRoutesDismissAndAutoplay() {
        var events: [String] = []
        PlayerNaturalEndAction.dismiss.perform(
            dismiss: { events.append("dismiss") },
            findNext: { events.append("next") }
        )
        PlayerNaturalEndAction.findNext.perform(
            dismiss: { events.append("dismiss") },
            findNext: { events.append("next") }
        )
        XCTAssertEqual(events, ["dismiss", "next"])
    }

    @MainActor
    func testPlayerFinishTearsDownBeforeDismissAndIsIdempotent() async {
        let lifecycle = PlayerLifecycleCoordinator()
        var events: [String] = []
        let dismissed = expectation(description: "dismissed after teardown")

        lifecycle.finish(
            teardown: { events.append("teardown") },
            completion: {
                events.append("dismiss")
                dismissed.fulfill()
            }
        )

        XCTAssertTrue(lifecycle.isTearingDown)
        XCTAssertEqual(events, ["teardown"])
        await fulfillment(of: [dismissed], timeout: 1)
        XCTAssertEqual(events, ["teardown", "dismiss"])

        let duplicateDismissed = expectation(description: "late completion delivered")
        lifecycle.finish(
            teardown: { events.append("duplicate teardown") },
            completion: {
                events.append("duplicate dismiss")
                duplicateDismissed.fulfill()
            }
        )
        lifecycle.teardown { events.append("disappear teardown") }
        await fulfillment(of: [duplicateDismissed], timeout: 1)
        XCTAssertEqual(
            events,
            ["teardown", "dismiss", "duplicate dismiss"],
            "a late caller's completion must run even though cleanup remains idempotent"
        )
    }

    @MainActor
    func testPlayerFinishCompletionSurvivesCoordinatorRelease() async {
        var lifecycle: PlayerLifecycleCoordinator? = PlayerLifecycleCoordinator()
        let completed = expectation(description: "completion survives owner release")
        lifecycle?.finish(teardown: {}, completion: { completed.fulfill() })
        lifecycle = nil
        await fulfillment(of: [completed], timeout: 1)
    }

    @MainActor
    func testPlayerStopReleasesTheCurrentItemEvenWhenRepeated() {
        let controller = PlayerController()
        let item = AVPlayerItem(url: URL(fileURLWithPath: "/dev/null"))
        controller.player.replaceCurrentItem(with: item)
        controller.showPlaybackNotice("Closing")

        controller.stop()

        XCTAssertNil(controller.player.currentItem)
        XCTAssertNil(controller.playbackNotice)
        XCTAssertFalse(controller.isPlaying)

        controller.stop()
        XCTAssertNil(controller.player.currentItem)
    }

    #if os(iOS)
    func testIOSSystemChromeWaitsForControlsAndPersistentContentToLeave() {
        for (controlsVisible, persistentContentVisible) in [
            (true, false),
            (false, true),
            (true, true),
        ] {
            let active = PlayerSystemOverlayPreferences.resolve(
                controlsVisible: controlsVisible,
                persistentContentVisible: persistentContentVisible
            )
            XCTAssertFalse(active.statusBarHidden)
            XCTAssertEqual(active.persistentOverlays, .automatic)
        }

        let idle = PlayerSystemOverlayPreferences.resolve(
            controlsVisible: false,
            persistentContentVisible: false
        )
        XCTAssertTrue(idle.statusBarHidden)
        XCTAssertEqual(idle.persistentOverlays, .hidden)

        XCTAssertFalse(
            PlayerSystemOverlayPreferences.restoredAfterPlayback.statusBarHidden
        )
        XCTAssertEqual(
            PlayerSystemOverlayPreferences.restoredAfterPlayback.persistentOverlays,
            .automatic
        )
    }

    @MainActor
    func testIOSPlayerSystemOverlayModifierReachesHostingController() {
        let preferences = PlayerSystemOverlayPreferences.resolve(
            controlsVisible: false,
            persistentContentVisible: false
        )
        let controller = UIHostingController(rootView:
            Color.clear.modifier(
                PlayerSystemOverlayModifier(preferences: preferences)
            )
        )
        controller.view.frame = CGRect(x: 0, y: 0, width: 1_024, height: 768)
        let window = UIWindow(frame: controller.view.frame)
        window.rootViewController = controller
        window.makeKeyAndVisible()
        controller.view.setNeedsLayout()
        controller.view.layoutIfNeeded()
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.2))

        XCTAssertTrue(controller.prefersStatusBarHidden)
        XCTAssertTrue(controller.prefersHomeIndicatorAutoHidden)
        window.isHidden = true
    }
    #endif

    func testNowPlayingSummaryUsesLoadedOverviewAndHasAFallback() {
        XCTAssertEqual(
            PlayerView.nowPlayingSummary("  A family crosses the stars.\n"),
            "A family crosses the stars."
        )
        XCTAssertEqual(PlayerView.nowPlayingSummary("  "), "No description available.")
        XCTAssertEqual(PlayerView.nowPlayingSummary(nil), "No description available.")
    }

    func testPlayerFormatsTheReleaseDateLikeTheWebOverlay() {
        XCTAssertEqual(
            PlayerView.playbackDateLabel(airDate: "2026-06-10", year: 2025),
            "Jun 10, 2026"
        )
        XCTAssertEqual(PlayerView.playbackDateLabel(airDate: nil, year: 2025), "2025")
    }

    #if os(iOS)
    @MainActor
    func testIPadPlaybackWideRowExpandsTheTimelineAcrossThePlayer() {
        let viewportWidth: CGFloat = 1_366
        let horizontalPadding: CGFloat = 20
        var timelineWidth: CGFloat = 0
        var rowFrame: CGRect = .null
        let controller = UIHostingController(rootView:
            PlayerTouchWideRow {
                Color.clear.frame(width: 132, height: 44)
            } timeline: {
                Color.clear
                    .frame(minWidth: 100, maxWidth: .infinity, minHeight: 44)
                    .reportLayoutWidth()
            } options: {
                Color.clear.frame(width: 300, height: 44)
            }
            .reportLayoutFrame()
            .padding(.horizontal, horizontalPadding)
            .onPreferenceChange(LayoutWidthPreferenceKey.self) {
                timelineWidth = $0
            }
            .onPreferenceChange(LayoutFramePreferenceKey.self) {
                rowFrame = $0
            }
        )

        controller.view.frame = CGRect(
            origin: .zero,
            size: CGSize(width: viewportWidth, height: 100)
        )
        let window = UIWindow(frame: controller.view.frame)
        window.rootViewController = controller
        window.makeKeyAndVisible()
        controller.view.setNeedsLayout()
        controller.view.layoutIfNeeded()
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.2))

        XCTAssertFalse(rowFrame.isNull)
        XCTAssertEqual(
            rowFrame.width,
            viewportWidth - (2 * horizontalPadding),
            accuracy: 0.5
        )
        XCTAssertGreaterThan(timelineWidth, viewportWidth / 2)
        window.isHidden = true
    }

    @MainActor
    func testSkipMarkerRowUsesProductionContentAtEveryTouchWidthAndTextSize() {
        let horizontalPadding: CGFloat = 20
        let sizeCategories: [ContentSizeCategory] = [
            .large,
            .accessibilityExtraExtraExtraLarge,
        ]

        for sizeCategory in sizeCategories {
            var referenceButtonWidth: CGFloat?

            for viewportWidth: CGFloat in [320, 390, 430, 744, 1_024, 1_366] {
                var buttonFrame: CGRect = .null
                let controller = UIHostingController(rootView:
                    PlayerTrailingControlRow {
                        Button {} label: {
                            PlayerMarkerButtonLabel(title: "Skip Credits")
                        }
                        .buttonStyle(.borderedProminent)
                        .reportLayoutFrame()
                    }
                    .environment(\.sizeCategory, sizeCategory)
                    .padding(.horizontal, horizontalPadding)
                    .onPreferenceChange(LayoutFramePreferenceKey.self) {
                        buttonFrame = $0
                    }
                )

                controller.view.frame = CGRect(
                    origin: .zero,
                    size: CGSize(width: viewportWidth, height: 160)
                )
                let window = UIWindow(frame: controller.view.frame)
                window.rootViewController = controller
                window.makeKeyAndVisible()
                controller.view.setNeedsLayout()
                controller.view.layoutIfNeeded()
                RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.05))

                let context = "width: \(viewportWidth), size: \(sizeCategory)"
                XCTAssertFalse(buttonFrame.isNull, context)
                XCTAssertEqual(
                    buttonFrame.maxX,
                    viewportWidth - horizontalPadding,
                    accuracy: 0.5,
                    context
                )
                XCTAssertLessThanOrEqual(
                    buttonFrame.width,
                    viewportWidth - (2 * horizontalPadding) + 0.5,
                    context
                )
                if sizeCategory == .large {
                    XCTAssertLessThan(buttonFrame.width, 180, context)
                    if let referenceButtonWidth {
                        XCTAssertEqual(
                            buttonFrame.width,
                            referenceButtonWidth,
                            accuracy: 0.5,
                            context
                        )
                    } else {
                        referenceButtonWidth = buttonFrame.width
                    }
                }
                window.isHidden = true
            }
        }
    }
    #endif

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
    func testIOSExposesDownloadsAndUsesConservativeDefaults() {
        XCTAssertEqual(
            HomeLayoutPolicy.topLevelTabs,
            ["Home", "Libraries", "Search", "Downloads", "Settings"]
        )
        XCTAssertEqual(OfflineQuality.standard.maximumHeight, 720)
        XCTAssertEqual(OfflineQuality.high.maximumHeight, 1_080)
        XCTAssertEqual(OfflineNetworkPolicy.wifiOnly.label, "Wi-Fi only")
    }

    func testPlayerOptionMenuPaletteStaysReadableInLightAndDarkAppearances() {
        for style in [UIUserInterfaceStyle.light, .dark] {
            let traits = UITraitCollection(userInterfaceStyle: style)
            let foreground = PlayerOptionMenuPalette.foreground.resolvedColor(with: traits)
            let secondary = PlayerOptionMenuPalette.secondaryForeground.resolvedColor(with: traits)
            let background = UIColor.systemBackground.resolvedColor(with: traits)

            XCTAssertGreaterThanOrEqual(
                contrastRatio(foreground, background),
                4.5,
                "primary labels must remain readable in \(style) mode"
            )
            XCTAssertGreaterThanOrEqual(
                contrastRatio(secondary, background),
                3,
                "section labels must remain readable in \(style) mode"
            )
        }
    }

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

    func testPhoneDetailUsesAnIntegratedHeroAndCompactControls() {
        XCTAssertGreaterThanOrEqual(IOSDetailMetrics.compactHeroHeight, 260)
        XCTAssertLessThanOrEqual(IOSDetailMetrics.compactHeroHeight, 300)
        XCTAssertGreaterThanOrEqual(IOSDetailMetrics.primaryControlHeight, 44)
        XCTAssertLessThanOrEqual(IOSDetailMetrics.primaryControlHeight, 48)
        XCTAssertGreaterThanOrEqual(IOSDetailMetrics.secondaryControlHeight, 44)
        XCTAssertEqual(IOSDetailMetrics.iconControlSize, 46)
    }

    func testCompactDetailKeepsThePrimaryActionOnItsOwnRow() {
        XCTAssertTrue(
            IOSDetailActionLayout.stacksPrimaryAction(horizontalSizeClass: .compact),
            "phone Resume text and timestamp must not share width with secondary actions"
        )
        XCTAssertTrue(
            IOSDetailActionLayout.stacksPrimaryAction(horizontalSizeClass: nil),
            "an unresolved compact environment should choose the safe stacked layout"
        )
        XCTAssertFalse(
            IOSDetailActionLayout.stacksPrimaryAction(horizontalSizeClass: .regular),
            "iPad has enough width for the established single-row layout"
        )
    }

    func testPhoneMetadataUsesCompactTextWithoutMediaPictograms() {
        let badges = [
            ItemMetadataBadge(
                kind: .year,
                symbol: "calendar",
                mark: "2025",
                accessibilityLabel: "2025"
            ),
            ItemMetadataBadge(
                kind: .runtime,
                symbol: "clock.fill",
                mark: "1 hr 58 min",
                accessibilityLabel: "1 hr 58 min"
            ),
            ItemMetadataBadge(
                kind: .resolution,
                symbol: "4k.tv.fill",
                mark: nil,
                accessibilityLabel: "4K"
            ),
        ]

        XCTAssertEqual(
            badges.map(ItemMetadataBadgeRow.compactLabel(for:)),
            ["2025", "1h 58m", "4K"]
        )
        XCTAssertEqual(
            badges.map(ItemMetadataBadgeRow.usesStyledMediaBadge(_:)),
            [false, false, true]
        )
    }

    func testPhoneResumeProgressIsClampedAndRuntimeIsTerse() {
        XCTAssertEqual(DetailView.compactRuntimeLabel(7_080_000), "1h 58m")
        XCTAssertEqual(DetailView.resumeFraction(positionMs: -1, durationMs: 100), 0)
        XCTAssertEqual(DetailView.resumeFraction(positionMs: 25, durationMs: 100), 0.25)
        XCTAssertEqual(DetailView.resumeFraction(positionMs: 200, durationMs: 100), 1)
    }

    func testPhoneHomeUsesOneCompactContinueFeature() {
        let first = Item(id: 1, kind: "movie", title: "First")
        let second = Item(id: 2, kind: "movie", title: "Second")

        XCTAssertTrue(HomeLayoutPolicy.usesFeaturedHero)
        XCTAssertEqual(
            HomeLayoutPolicy.continueWatchingShelfItems([first, second]).map(\.id),
            [2]
        )
        XCTAssertLessThanOrEqual(HomeHeroMetrics.compactHeight, 250)
        XCTAssertGreaterThanOrEqual(HomeHeroMetrics.cornerRadius, 16)
    }

    @MainActor
    func testPhoneHomeHeroKeepsEqualInsetsOnBothEdges() {
        for viewportWidth: CGFloat in [375, 430] {
            var heroFrame: CGRect = .null
            let controller = UIHostingController(rootView:
                Color.clear
                    .frame(maxWidth: .infinity, minHeight: HomeHeroMetrics.compactHeight)
                    .reportLayoutFrame()
                    .modifier(IOSHomeHeroLayout(compact: true))
                    .onPreferenceChange(LayoutFramePreferenceKey.self) {
                        heroFrame = $0
                    }
            )

            controller.view.frame = CGRect(
                origin: .zero,
                size: CGSize(width: viewportWidth, height: HomeHeroMetrics.compactHeight)
            )
            let window = UIWindow(frame: controller.view.frame)
            window.rootViewController = controller
            window.makeKeyAndVisible()
            controller.view.setNeedsLayout()
            controller.view.layoutIfNeeded()
            RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.1))

            XCTAssertFalse(heroFrame.isNull)
            XCTAssertEqual(heroFrame.minX, HomeHeroMetrics.horizontalInset, accuracy: 0.5)
            XCTAssertEqual(
                heroFrame.maxX,
                viewportWidth - HomeHeroMetrics.horizontalInset,
                accuracy: 0.5
            )
            window.isHidden = true
        }
    }

    private func contrastRatio(_ foreground: UIColor, _ background: UIColor) -> CGFloat {
        let lighter = max(relativeLuminance(foreground), relativeLuminance(background))
        let darker = min(relativeLuminance(foreground), relativeLuminance(background))
        return (lighter + 0.05) / (darker + 0.05)
    }

    private func relativeLuminance(_ color: UIColor) -> CGFloat {
        var red: CGFloat = 0
        var green: CGFloat = 0
        var blue: CGFloat = 0
        var alpha: CGFloat = 0
        XCTAssertTrue(color.getRed(&red, green: &green, blue: &blue, alpha: &alpha))

        func linear(_ component: CGFloat) -> CGFloat {
            component <= 0.04045
                ? component / 12.92
                : pow((component + 0.055) / 1.055, 2.4)
        }

        return 0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
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

        XCTAssertEqual(cardShelfMetadata(episode), "S4 E2  44m left")
        XCTAssertEqual(cardShelfMetadata(movie), "2025  115m left")
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

        XCTAssertEqual(continueWatchingDetail(episode), "S4 E2  Fray")
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
        let format = UIGraphicsImageRendererFormat.preferred()
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

    func testArtworkCacheIdentityAndFreshnessFollowTheSourceURL() {
        XCTAssertNotEqual(
            AuthImageCache.sourceKey(origin: "http://a:32400", path: "/images/one.jpg"),
            AuthImageCache.sourceKey(origin: "http://a:32400", path: "/images/two.jpg")
        )
        XCTAssertNotEqual(
            AuthImageCache.sourceKey(origin: "http://a:32400", path: "/images/one.jpg"),
            AuthImageCache.sourceKey(origin: "http://b:32400", path: "/images/one.jpg")
        )
        XCTAssertEqual(
            AuthImageCache.sourceKey(origin: "http://a:32400", path: "https://cdn.test/a.jpg"),
            AuthImageCache.sourceKey(origin: "http://b:32400", path: "https://cdn.test/a.jpg")
        )

        let now = Date(timeIntervalSince1970: 2_000_000_000)
        XCTAssertTrue(
            AuthImageCache.isFresh(
                storedAt: now.addingTimeInterval(-AuthImageCache.freshAge),
                now: now
            )
        )
        XCTAssertFalse(
            AuthImageCache.isFresh(
                storedAt: now.addingTimeInterval(-AuthImageCache.freshAge - 1),
                now: now
            )
        )
    }

    func testArtworkHTTPFailureClassificationPreservesStaleFallbacks() {
        for status in [200, 204, 299] {
            XCTAssertEqual(AuthImageCache.classify(statusCode: status), .success)
        }
        for status in [401, 403, 404, 410] {
            XCTAssertEqual(AuthImageCache.classify(statusCode: status), .terminalFailure)
        }
        for status in [300, 400, 408, 429, 500, 502, 599] {
            XCTAssertEqual(AuthImageCache.classify(statusCode: status), .transientFailure)
        }
    }

    func testArtworkDiskCacheSurvivesRecreationAndExpiresOldEntries() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("plurx-artwork-tests-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let now = Date(timeIntervalSince1970: 2_000_000_000)
        let bytes = Data("persistent artwork".utf8)
        let first = AuthImageDiskCache(
            directory: directory,
            byteLimit: 10_000,
            maximumStaleAge: 30 * 24 * 60 * 60
        )
        await first.store(bytes, for: "http://server/images/poster.jpg", storedAt: now)

        // A new cache object models the next app process opening the same
        // Caches directory.
        let reopened = AuthImageDiskCache(
            directory: directory,
            byteLimit: 10_000,
            maximumStaleAge: 30 * 24 * 60 * 60
        )
        let persisted = await reopened.entry(
            for: "http://server/images/poster.jpg",
            now: now.addingTimeInterval(60)
        )
        XCTAssertEqual(persisted?.data, bytes)
        XCTAssertEqual(persisted?.storedAt, now)

        let expiredKey = "http://server/images/expired.jpg"
        await reopened.store(
            bytes,
            for: expiredKey,
            storedAt: now.addingTimeInterval(-(31 * 24 * 60 * 60))
        )
        let expired = await reopened.entry(for: expiredKey, now: now)
        XCTAssertNil(expired)
    }

    func testArtworkDiskCacheEvictsTheLeastRecentlyUsedEntryAtItsByteLimit() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("plurx-artwork-lru-tests-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = AuthImageDiskCache(
            directory: directory,
            byteLimit: 3_000,
            maximumStaleAge: 30 * 24 * 60 * 60
        )
        let now = Date(timeIntervalSince1970: 2_000_000_000)
        let bytes = Data(repeating: 7, count: 1_200)
        await cache.store(bytes, for: "first", storedAt: now.addingTimeInterval(-3))
        await cache.store(bytes, for: "second", storedAt: now.addingTimeInterval(-2))
        _ = await cache.entry(for: "first", now: now.addingTimeInterval(-1))
        await cache.store(bytes, for: "third", storedAt: now)

        let first = await cache.entry(for: "first", now: now)
        let second = await cache.entry(for: "second", now: now)
        let third = await cache.entry(for: "third", now: now)
        XCTAssertNotNil(first)
        XCTAssertNil(second)
        XCTAssertNotNil(third)
        XCTAssertEqual(AuthImageDiskCache.token(for: "first").count, 64)
        XCTAssertNotEqual(
            AuthImageDiskCache.token(for: "first"),
            AuthImageDiskCache.token(for: "third")
        )
    }

    func testArtworkDiskCacheRejectsAnInflightStoreAfterClear() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("plurx-artwork-generation-tests-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = AuthImageDiskCache(
            directory: directory,
            byteLimit: 10_000,
            maximumStaleAge: 30 * 24 * 60 * 60
        )
        let oldGeneration = await cache.currentGeneration()
        await cache.removeAll()

        let accepted = await cache.store(
            Data("old server artwork".utf8),
            for: "http://old-server/images/poster.jpg",
            generation: oldGeneration
        )

        XCTAssertFalse(accepted)
        let entry = await cache.entry(for: "http://old-server/images/poster.jpg")
        XCTAssertNil(entry)
    }

    func testArtworkCacheClearIsWiredToTheDiskCache() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("plurx-artwork-clear-tests-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let diskCache = AuthImageDiskCache(
            directory: directory,
            byteLimit: 10_000,
            maximumStaleAge: 30 * 24 * 60 * 60
        )
        let session = URLSession(configuration: .ephemeral)
        defer { session.invalidateAndCancel() }
        let cache = AuthImageCache(diskCache: diskCache, session: session)
        let source = "http://server/images/poster.jpg"
        await diskCache.store(Data("private artwork".utf8), for: source)

        cache.clear()

        var entry = await diskCache.entry(for: source)
        for _ in 0..<100 where entry != nil {
            try? await Task.sleep(nanoseconds: 10_000_000)
            entry = await diskCache.entry(for: source)
        }
        XCTAssertNil(entry)
    }

    func testArtworkDownloadCoalescingCancelsOnlyAfterTheLastWaiterLeaves() async {
        let coordinator = AuthImageDownloadCoordinator()
        let probe = ArtworkDownloadProbe()
        let key = "http://server/images/poster.jpg"
        let first = Task {
            await coordinator.download(key) { await probe.runUntilCancelled() }
        }

        var waiters = 0
        for _ in 0..<100 where waiters != 1 {
            waiters = await coordinator.waiterCount(for: key)
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(waiters, 1)

        let second = Task {
            await coordinator.download(key) { await probe.runUntilCancelled() }
        }
        for _ in 0..<100 where waiters != 2 {
            waiters = await coordinator.waiterCount(for: key)
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(waiters, 2)

        first.cancel()
        for _ in 0..<100 where waiters != 1 {
            waiters = await coordinator.waiterCount(for: key)
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        let afterFirstCancel = await probe.snapshot()
        XCTAssertEqual(waiters, 1)
        XCTAssertEqual(afterFirstCancel.cancellations, 0)

        second.cancel()
        let firstResult = await first.value
        let secondResult = await second.value
        let final = await probe.snapshot()
        if case .transientFailure = firstResult {} else {
            XCTFail("cancelled shared download should be transient")
        }
        if case .transientFailure = secondResult {} else {
            XCTFail("cancelled shared download should be transient")
        }
        XCTAssertEqual(final.starts, 1)
        XCTAssertEqual(final.cancellations, 1)
        let finalWaiters = await coordinator.waiterCount(for: key)
        XCTAssertEqual(finalWaiters, 0)
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

    func testPGSOverlayManifestDecodesAndRejectsIdentityOrTraversalDrift() throws {
        let generation = String(repeating: "a", count: 64)
        let json = Data(#"""
        {
          "schema": 1,
          "generation": "\#(generation)",
          "file_id": 42,
          "track_index": 3,
          "kind": "pgs",
          "timebase": "source_ms",
          "duration_ms": 120000,
          "cues": [{
            "id": "c00000001",
            "start_ms": 1000,
            "end_ms": 2500,
            "canvas_width": 1920,
            "canvas_height": 1080,
            "objects": [{
              "image": "overlay/\#(generation)/objects/\#(generation).png",
              "x": 240,
              "y": 850,
              "width": 1440,
              "height": 180
            }]
          }]
        }
        """#.utf8)
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let manifest = try decoder.decode(PGSOverlayManifest.self, from: json)

        XCTAssertNoThrow(try manifest.validated(fileId: 42, trackIndex: 3))
        XCTAssertThrowsError(try manifest.validated(fileId: 41, trackIndex: 3))

        let escaped = PGSOverlayObject(
            image: "overlay/\(generation)/objects/../secret.png",
            x: 0, y: 0, width: 1, height: 1
        )
        XCTAssertNil(PGSOverlayManifest.objectHash(
            from: escaped.image,
            generation: generation
        ))

        let overflowGeometry = PGSOverlayObject(
            image: "overlay/\(generation)/objects/\(generation).png",
            x: Int.max,
            y: 0,
            width: Int.max,
            height: 1
        )
        let invalidGeometry = PGSOverlayManifest(
            schema: 1,
            generation: generation,
            fileId: 42,
            trackIndex: 3,
            kind: "pgs",
            timebase: "source_ms",
            durationMs: 120_000,
            cues: [PGSOverlayCue(
                id: "c1",
                startMs: 1_000,
                endMs: 2_000,
                canvasWidth: 1_920,
                canvasHeight: 1_080,
                objects: [overflowGeometry]
            )]
        )
        XCTAssertThrowsError(try invalidGeometry.validated(fileId: 42, trackIndex: 3))

        let dimensionDrift = PGSOverlayManifest(
            schema: 1,
            generation: generation,
            fileId: 42,
            trackIndex: 3,
            kind: "pgs",
            timebase: "source_ms",
            durationMs: 120_000,
            cues: [
                PGSOverlayCue(
                    id: "c1",
                    startMs: 1_000,
                    endMs: 2_000,
                    canvasWidth: 1_920,
                    canvasHeight: 1_080,
                    objects: [PGSOverlayObject(
                        image: "overlay/\(generation)/objects/\(generation).png",
                        x: 0, y: 0, width: 100, height: 50
                    )]
                ),
                PGSOverlayCue(
                    id: "c2",
                    startMs: 3_000,
                    endMs: 4_000,
                    canvasWidth: 1_920,
                    canvasHeight: 1_080,
                    objects: [PGSOverlayObject(
                        image: "overlay/\(generation)/objects/\(generation).png",
                        x: 0, y: 0, width: 101, height: 50
                    )]
                ),
            ]
        )
        XCTAssertThrowsError(try dimensionDrift.validated(fileId: 42, trackIndex: 3))
    }

    func testPGSOverlayUsesSourceTimeWithANonZeroItemBase() {
        XCTAssertEqual(PGSOverlayPolicy.itemTimeMs(sourceTimeMs: 93_500, baseMs: 90_000), 3_500)
        XCTAssertEqual(PGSOverlayPolicy.itemTimeMs(sourceTimeMs: 89_000, baseMs: 90_000), -1_000)
        XCTAssertEqual(
            PGSOverlayPolicy.windowRange(at: 95_000, durationMs: 120_000),
            90_000..<120_000
        )
        XCTAssertFalse(PGSOverlayPolicy.shouldRefresh(
            sourceTimeMs: 95_000,
            loadedRange: 90_000..<120_000
        ))
        XCTAssertTrue(PGSOverlayPolicy.shouldRefresh(
            sourceTimeMs: 101_000,
            loadedRange: 90_000..<120_000
        ))

        let duplicate = PGSOverlayObject(
            image: "same", x: 0, y: 0, width: 4_096, height: 2_160
        )
        let cue = PGSOverlayCue(
            id: "c1",
            startMs: 0,
            endMs: 1,
            canvasWidth: 4_096,
            canvasHeight: 2_160,
            objects: [duplicate, duplicate]
        )
        XCTAssertTrue(PGSOverlayPolicy.windowFitsDecodedBudget([cue]))
        XCTAssertFalse(PGSOverlayPolicy.windowFitsDecodedBudget([cue, PGSOverlayCue(
            id: "c2",
            startMs: 2,
            endMs: 3,
            canvasWidth: 4_096,
            canvasHeight: 2_160,
            objects: [PGSOverlayObject(
                image: "different", x: 0, y: 0, width: 4_096, height: 2_160
            ), PGSOverlayObject(
                image: "third", x: 0, y: 0, width: 4_096, height: 2_160
            )]
        )]))
    }

    func testPGSOverlayLayoutMaps1080pOnto4KAndLetterboxedDestinations() {
        let object = PGSOverlayObject(
            image: "ignored", x: 100, y: 800, width: 500, height: 200
        )
        XCTAssertEqual(
            PGSOverlayPolicy.objectFrame(
                object,
                canvasWidth: 1920,
                canvasHeight: 1080,
                destination: CGRect(x: 0, y: 0, width: 3840, height: 2160)
            ),
            CGRect(x: 200, y: 1600, width: 1000, height: 400)
        )

        let fourThree = PGSOverlayPolicy.objectFrame(
            object,
            canvasWidth: 1920,
            canvasHeight: 1080,
            destination: CGRect(x: 0, y: 0, width: 1024, height: 768)
        )
        XCTAssertEqual(fourThree.minX, 53.333, accuracy: 0.01)
        XCTAssertEqual(fourThree.minY, 522.667, accuracy: 0.01)
        XCTAssertEqual(fourThree.width, 266.667, accuracy: 0.01)
        XCTAssertEqual(fourThree.height, 106.667, accuracy: 0.01)

        let dvdObject = PGSOverlayObject(
            image: "ignored", x: 0, y: 400, width: 720, height: 80
        )
        let anamorphic = PGSOverlayPolicy.objectFrame(
            dvdObject,
            canvasWidth: 720,
            canvasHeight: 480,
            destination: CGRect(x: 0, y: 0, width: 1920, height: 1080)
        )
        XCTAssertEqual(anamorphic, CGRect(x: 150, y: 900, width: 1620, height: 180))
    }

    @MainActor
    func testPGSOverlaySelectionNeverBurnsOrReopensDirectVideo() {
        let overlay = SubtitleTrack(
            index: 0,
            codec: "hdmv_pgs_subtitle",
            language: "eng",
            title: "PGS",
            default: false,
            forced: false,
            text: false,
            native: false,
            overlay: "pgs-v1"
        )
        let native = SubtitleTrack(
            index: 1,
            codec: "subrip",
            language: "eng",
            title: "Text",
            default: false,
            forced: false,
            text: true,
            native: true,
            overlay: nil
        )
        let unknown = SubtitleTrack(
            index: 2,
            codec: "hdmv_pgs_subtitle",
            language: "eng",
            title: "Future",
            default: false,
            forced: false,
            text: false,
            native: false,
            overlay: "pgs-v2"
        )
        let tracks = [overlay, native, unknown]

        XCTAssertTrue(PlayerController.subtitleUsesOverlay(0, in: tracks))
        XCTAssertFalse(PlayerController.subtitleRequiresBurn(0, in: tracks))
        XCTAssertTrue(PlayerController.subtitleRequiresBurn(2, in: tracks))
        XCTAssertFalse(PlayerController.subtitleBurnWouldDiscardHDR(
            0,
            tracks: tracks,
            deliveredRange: "dolby_vision"
        ))
        let fields = PlayerController.sessionSubtitleFields(
            selected: 0,
            tracks: tracks,
            legacyBurn: true
        )
        XCTAssertNil(fields.burn)
        XCTAssertNil(fields.native)
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 0,
                tracks: tracks,
                activeBurn: nil,
                isDirectPlayback: true
            ),
            .bitmapOverlay
        )
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: nil,
                tracks: tracks,
                activeBurn: nil,
                isDirectPlayback: true,
                activeOverlay: 0
            ),
            .bitmapOverlay
        )
        XCTAssertEqual(
            PlayerController.subtitleSelectionRoute(
                for: 1,
                tracks: tracks,
                activeBurn: nil,
                isDirectPlayback: true,
                activeOverlay: 0
            ),
            .reopen
        )
    }

    @MainActor
    func testPGSOverlayReattachesToAReplacementPlayerItem() throws {
        let format = UIGraphicsImageRendererFormat.preferred()
        format.scale = 1
        let image = UIGraphicsImageRenderer(
            size: CGSize(width: 2, height: 2),
            format: format
        ).image { context in
            UIColor.white.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 2, height: 2))
        }
        let object = PGSOverlayObject(
            image: "overlay", x: 0, y: 0, width: 2, height: 2
        )
        let cue = PGSOverlayCue(
            id: "c1",
            startMs: 10_000,
            endMs: 12_000,
            canvasWidth: 1920,
            canvasHeight: 1080,
            objects: [object]
        )
        let window = PGSOverlayWindow(
            revision: 7,
            generation: String(repeating: "a", count: 64),
            baseMs: 9_000,
            sourceRange: 9_000..<20_000,
            cues: [PGSOverlayRenderableCue(
                cue: cue,
                objects: [PGSOverlayRenderableObject(
                    object: object,
                    image: try XCTUnwrap(image.cgImage)
                )]
            )]
        )
        let first = AVPlayerItem(url: URL(string: "https://example.invalid/first.mp4")!)
        let second = AVPlayerItem(url: URL(string: "https://example.invalid/second.mp4")!)
        let surface = PlayerSurfaceView(frame: CGRect(x: 0, y: 0, width: 1920, height: 1080))

        surface.applyPGSOverlay(window, to: first)
        XCTAssertTrue(surface.hasPGSOverlay(revision: 7, item: first))
        XCTAssertEqual(surface.pgsOverlayNodeCount, 1)

        surface.applyPGSOverlay(window, to: second)
        XCTAssertFalse(surface.hasPGSOverlay(revision: 7, item: first))
        XCTAssertTrue(surface.hasPGSOverlay(revision: 7, item: second))
        XCTAssertEqual(surface.pgsOverlayNodeCount, 1)

        surface.applyPGSOverlay(nil, to: nil)
        XCTAssertEqual(surface.pgsOverlayNodeCount, 0)
    }
}
