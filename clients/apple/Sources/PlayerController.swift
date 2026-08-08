import AVKit
import Combine
import Foundation
#if os(iOS)
import MediaPlayer
#endif

private enum PlaybackPreparationError: LocalizedError {
    case timedOut
    case failed

    var errorDescription: String? {
        switch self {
        case .timedOut:
            return "The video took too long to prepare. Check that the Cinema server is reachable."
        case .failed:
            return "The video stream could not be prepared. Check the server and try again."
        }
    }
}

/// One AVPlayer failure forwarded to the server's bounded client-log endpoint.
/// Keeping this payload small and token-free makes failures visible in the
/// ordinary server log without retaining a media capability URL.
private struct ApplePlaybackFailureLog: Encodable {
    let level = "error"
    let event = "avplayer_item_failed"
    let message: String
    let method: String
    let code: Int?
    let title: String
    let fileId: Int
    let vcodec: String?
    let detail: String
    let ua = "Apple AVPlayer"

    enum CodingKeys: String, CodingKey {
        case level, event, message, method, code, title, detail, ua
        case fileId = "file_id"
        case vcodec
    }
}

/// One sustained AVPlayer clock stall forwarded before the controller either
/// reconnects the same delivery or stops after an immediate repeat. The
/// server's existing `stall` vocabulary renders `ms` as `stall_ms`; position
/// and outcome stay in the bounded detail field because the endpoint has no
/// dedicated film-position column.
struct ApplePlaybackStallLog: Encodable {
    let level = "warn"
    let event = "stall"
    let message: String
    let method: String
    let title: String
    let fileId: Int
    let vcodec: String?
    let detail: String
    let ms: Int
    let encoder: String?
    let ua = "Apple AVPlayer"

    init(
        kind: PlaybackStallKind,
        outcome: SameDeliveryStallRecoveryOutcome,
        positionMs: Int,
        durationMs: Int,
        method: String,
        title: String,
        fileId: Int,
        vcodec: String?,
        encoder: String?
    ) {
        message = kind == .buffering
            ? "AVPlayer buffering wait triggered same-delivery recovery"
            : "AVPlayer stopped advancing and triggered same-delivery recovery"
        self.method = method
        self.title = title
        self.fileId = fileId
        self.vcodec = vcodec
        detail = "kind=\(kind.rawValue) · position_ms=\(max(0, positionMs)) · outcome=\(outcome.rawValue)"
        ms = max(0, durationMs)
        self.encoder = encoder
    }

    enum CodingKeys: String, CodingKey {
        case level, event, message, method, title, detail, ms, encoder, ua
        case fileId = "file_id"
        case vcodec
    }
}

/// Requests that arrive while a stream replacement is in flight are remembered
/// rather than dropped. Two replacements must never overlap — they share a
/// `playback_id`, so the newer create intentionally removes the older session
/// and can otherwise strand AVPlayer on a URL the server has just deleted — but
/// a seek or track change issued during one is real viewer intent, and dropping
/// it snapped the tvOS progress bar back to where the change had started. Last
/// writer wins, and exactly one trailing reopen runs when the change lands.
///
/// A plain value with no player and no network in it, so the policy is
/// unit-testable without either.
struct PlayerReopenQueue: Equatable {
    private(set) var pendingMs: Int?

    /// The position to open right now, or `nil` when a replacement is already
    /// running and this request was queued behind it instead.
    mutating func request(_ positionMs: Int, changeInFlight: Bool) -> Int? {
        guard changeInFlight else { return positionMs }
        pendingMs = positionMs
        return nil
    }

    /// The single trailing reopen that accumulated during the change which just
    /// finished, consumed. Deliberately *not* deduplicated against the position
    /// just opened: a queued audio, quality, or subtitle change can name the
    /// very same position and still needs the session rebuilt around it.
    mutating func takePending() -> Int? {
        defer { pendingMs = nil }
        return pendingMs
    }

    /// The stream is gone — the change failed, or the player stopped. Nothing
    /// queued behind it is worth reopening.
    mutating func clear() { pendingMs = nil }
}

/// The film-time target of an interactive seek. AVPlayer's clock is not a
/// safe place to accumulate remote presses: a growing HLS session is replaced
/// for every seek, and while that replacement is in flight the old item's
/// local clock and the new session's film-time base briefly coexist. Keeping
/// the latest target here makes ten presses mean ten steps, regardless of
/// which item AVPlayer happens to expose between them.
struct PlayerSeekState: Equatable {
    private(set) var pendingMs: Int?
    private(set) var generation = 0

    mutating func absolute(_ requestedMs: Int, durationMs: Int) -> (target: Int, generation: Int) {
        generation &+= 1
        let target = Self.clamp(requestedMs, durationMs: durationMs)
        pendingMs = target
        return (target, generation)
    }

    mutating func relative(
        by deltaMs: Int,
        observedMs: Int,
        durationMs: Int
    ) -> (target: Int, generation: Int) {
        absolute((pendingMs ?? observedMs) + deltaMs, durationMs: durationMs)
    }

    /// Only the newest native AVPlayer seek may clear the optimistic target.
    /// An older completion can arrive after AVPlayer cancels it for a newer
    /// seek and must not snap the progress bar backward.
    @discardableResult
    mutating func complete(generation expected: Int) -> Bool {
        guard expected == generation else { return false }
        pendingMs = nil
        return true
    }

    /// Live HLS seeks are serialized by `PlayerReopenQueue`; the final open in
    /// that drain clears the target it actually attached.
    mutating func completeReopen(at positionMs: Int) {
        if pendingMs == positionMs { pendingMs = nil }
    }

    mutating func clear() { pendingMs = nil }

    private static func clamp(_ requestedMs: Int, durationMs: Int) -> Int {
        let upper = durationMs > 0 ? max(0, durationMs - 2_000) : Int.max
        return min(max(0, requestedMs), upper)
    }
}

enum PlaybackStallAction: Equatable {
    case none
    case nudge
    case reopen
}

enum PlaybackStallKind: String, Equatable {
    case silent
    case buffering

    var terminalState: PlaybackStallTerminalState {
        switch self {
        case .silent:
            return PlaybackStallTerminalState(
                message: "Playback stopped responding after retrying the current stream."
            )
        case .buffering:
            return PlaybackStallTerminalState(
                message: "Playback could not resume after repeated buffering. Check the connection and try again."
            )
        }
    }
}

struct PlaybackStallEvent: Equatable {
    let kind: PlaybackStallKind
    let action: PlaybackStallAction
    let positionMs: Int
    let durationMs: Int
}

struct PlaybackStallSelection: Equatable {
    let kind: PlaybackStallKind
    let action: PlaybackStallAction
}

struct PlaybackStallTerminalState: Equatable {
    let isPlaying = false
    let wantsPlayback = false
    let failed = true
    let message: String
}

enum SameDeliveryStallRecoveryOutcome: String, Equatable {
    case reopen
    case terminal
}

enum SameDeliveryStallRecoveryDecision: Equatable {
    case reopen
    case stop(PlaybackStallTerminalState)

    var outcome: SameDeliveryStallRecoveryOutcome {
        switch self {
        case .reopen: return .reopen
        case .stop: return .terminal
        }
    }
}

/// The same stream gets one reconnect. Five seconds of later film-clock
/// progress resets this state in the controller, so a new interruption may
/// recover while an immediate repeat stops instead of looping.
struct SameDeliveryStallRecoveryState: Equatable {
    private(set) var attempted = false

    mutating func next(for kind: PlaybackStallKind) -> SameDeliveryStallRecoveryDecision {
        guard !attempted else { return .stop(kind.terminalState) }
        attempted = true
        return .reopen
    }

    mutating func reset() { attempted = false }
}

enum PlaybackCompatibilityFallback: Equatable {
    case none
    case hdrBase
    case transcode
}

/// Monotonic elapsed-time sampling policy for AVPlayer's silent-wait failure mode. A
/// temporary buffer wait gets room to recover on its own; only sustained lack
/// of film-time progress rebuilds the item, which is the in-player equivalent
/// of the back-out-and-play-again workaround.
struct PlaybackStallDetector: Equatable {
    private(set) var lastPositionMs: Int?
    private(set) var stagnantChecks = 0
    private(set) var stagnantSince: TimeInterval?

    mutating func sample(
        positionMs: Int,
        shouldMonitor: Bool,
        observedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) -> PlaybackStallAction {
        guard shouldMonitor else {
            reset()
            return .none
        }
        guard let lastPositionMs else {
            self.lastPositionMs = positionMs
            stagnantSince = observedAt
            return .none
        }

        // Ordinary playback advances much farther than this between the
        // two-second samples. A backwards discontinuity means an item/seek
        // changed under the monitor and establishes a new baseline too.
        if positionMs >= lastPositionMs + 250 || positionMs < lastPositionMs - 250 {
            self.lastPositionMs = positionMs
            stagnantChecks = 0
            stagnantSince = observedAt
            return .none
        }

        stagnantChecks += 1
        if stagnantChecks >= 6 {
            self.lastPositionMs = positionMs
            stagnantChecks = 0
            return .reopen
        }
        return stagnantChecks == 3 ? .nudge : .none
    }

    func stagnantDurationMs(at observedAt: TimeInterval) -> Int {
        guard let stagnantSince else { return 0 }
        return max(0, Int(((observedAt - stagnantSince) * 1_000).rounded()))
    }

    mutating func reset() {
        lastPositionMs = nil
        stagnantChecks = 0
        stagnantSince = nil
    }
}

/// Owns both stall detectors so predicate gating, merge precedence, cause, and
/// measured duration are one testable policy rather than parallel expressions
/// inside an asynchronous AVPlayer loop.
struct PlaybackRecoveryMonitor: Equatable {
    private(set) var silentDetector = PlaybackStallDetector()
    private(set) var bufferingDetector = PlaybackStallDetector()

    @MainActor
    mutating func sample(
        positionMs: Int,
        timeControlStatus: AVPlayer.TimeControlStatus,
        shouldMonitor: Bool,
        establishedPlayback: Bool,
        observedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) -> PlaybackStallEvent? {
        let silentAction = silentDetector.sample(
            positionMs: positionMs,
            shouldMonitor: shouldMonitor
                && PlayerController.shouldMonitorSilentPlaybackStall(
                    timeControlStatus: timeControlStatus
                ),
            observedAt: observedAt
        )
        let bufferingAction = bufferingDetector.sample(
            positionMs: positionMs,
            shouldMonitor: shouldMonitor
                && establishedPlayback
                && PlayerController.shouldMonitorBufferingStall(
                    timeControlStatus: timeControlStatus
                ),
            observedAt: observedAt
        )
        guard let selection = Self.select(
            silentAction: silentAction,
            bufferingAction: bufferingAction
        ) else { return nil }
        let durationMs = selection.kind == .buffering
            ? bufferingDetector.stagnantDurationMs(at: observedAt)
            : silentDetector.stagnantDurationMs(at: observedAt)
        return PlaybackStallEvent(
            kind: selection.kind,
            action: selection.action,
            positionMs: positionMs,
            durationMs: durationMs
        )
    }

    /// Buffering wins if a future predicate change accidentally enables both
    /// legs. The current predicates are exclusive; pinning precedence keeps a
    /// later relaxation from routing a network wait into silent/HDR recovery.
    static func select(
        silentAction: PlaybackStallAction,
        bufferingAction: PlaybackStallAction
    ) -> PlaybackStallSelection? {
        if bufferingAction != .none {
            return PlaybackStallSelection(kind: .buffering, action: bufferingAction)
        }
        if silentAction != .none {
            return PlaybackStallSelection(kind: .silent, action: silentAction)
        }
        return nil
    }

    mutating func reset() {
        silentDetector.reset()
        bufferingDetector.reset()
    }
}

/// One entry of an AVPlayer legible media-selection group, reduced to the two
/// attributes the server actually authored (`LANGUAGE` and `NAME`). Keeping the
/// matching rule off AVFoundation types is what makes it testable.
struct SubtitleRenditionOption: Equatable {
    var languageTag: String?
    var displayName: String
}

/// What one subtitle selection costs: media selection inside the current item,
/// or a replacement server session.
enum SubtitleSelectionRoute: Equatable {
    case mediaSelection
    case bitmapOverlay
    case reopen
}

/// Drives one AVPlayer and executes the server-owned delivery plan. It also
/// supplies the controls AVPlayer withholds for a growing EVENT playlist: an
/// explicit on-demand timeline, reliable play/pause commands, server playback
/// telemetry, and stream restarts for audio, quality, and burn-only subtitle
/// changes. Ordinary text subtitles switch through AVPlayer media selection —
/// once the stream carries their renditions, which under the default
/// `SubtitleReadiness.onDemand` is from the first selection rather than from the
/// first frame (`needsNativeSubtitleSession`).
@MainActor
final class PlayerController: ObservableObject {
    /// The server retains 120 seconds behind the download frontier: 60 seconds
    /// for the client's forward fetch, 30 for back-buffering, and 30 for a
    /// retry. AVPlayer's default of zero lets it choose the forward fetch;
    /// once that passed the server's retention window, the reaper could delete
    /// media the player had fetched but had not presented yet. Keep growing
    /// HLS sessions inside the contract while leaving direct and completed-VOD
    /// items under AVPlayer's normal policy.
    static let growingHLSForwardBufferSeconds: TimeInterval = 60

    static func configureBuffering(_ item: AVPlayerItem, growingHLS: Bool) {
        item.preferredForwardBufferDuration = growingHLS
            ? growingHLSForwardBufferSeconds
            : 0
    }

    let player = AVPlayer()

    @Published private(set) var decision: Decision?
    @Published private(set) var sessionStatus: PlaybackSessionStatus?
    @Published private(set) var currentMs = 0
    @Published private(set) var knownDurationMs = 0
    @Published private(set) var isPlaying = false
    @Published private(set) var isChangingStream = false
    @Published private(set) var selectedSubtitle: Int?
    @Published private(set) var selectedAudio: Int?
    @Published private(set) var selectedHeight: Int?
    @Published private(set) var encoder: String?
    /// The dynamic range of the bytes this playback is actually receiving —
    /// `"dolby_vision" | "hdr10" | "hlg" | "sdr"`, or nil against a server that
    /// does not report it. Purely a readout: nothing in this controller ever
    /// reads it back, and no decision, capability, or session request depends
    /// on it (MEDIA-BADGES-PLAN.md §9).
    @Published private(set) var deliveredRange: String?
    @Published private(set) var isVOD = false
    @Published private(set) var failed = false
    @Published private(set) var playbackError: String?
    @Published private(set) var playbackNotice: String?
    @Published private(set) var finished = false
    @Published private(set) var pgsOverlayWindow: PGSOverlayWindow?
    @Published private(set) var pgsOverlayStatus: PGSOverlayStatus = .off

    private var baseMs = 0
    private var itemId = 0
    private var fileId = 0
    private var title = ""
    #if os(iOS)
    private var offlineId: String?
    #endif
    private var audioOverride: Int?
    private weak var model: AppModel?
    private var timeObserver: Any?
    private var endObserver: NSObjectProtocol?
    private var itemStatusObservation: NSKeyValueObservation?
    private var statusTask: Task<Void, Never>?
    private var recoveryTask: Task<Void, Never>?
    private var playbackNoticeTask: Task<Void, Never>?
    private var started = false
    private var sessionId: String?
    private var lastReportedMs = 0
    private var usesDirectTimeline = false
    private var canRetryCurrentItemWithHDRBase = false
    private var dolbyVisionFallbackAttempted = false
    private var forceCompatibleHDRBase = false
    private var canRetryCurrentItemWithTranscode = false
    private var compatibilityFallbackAttempted = false
    private var forceCompatibilityTranscode = false
    /// Once real playback has begun, a transport interruption is not evidence
    /// that the device rejected HDR. Rebuild the same delivery once instead of
    /// silently replacing a picture the viewer has already watched with SDR.
    private var establishedPlayback = false
    private var establishedHDRRetryAttempted = false
    private var attachedAtPositionMs = 0
    /// A burn is part of the current video frames. Leaving one requires one
    /// reopen; native-to-native and native-to-Off never do.
    private var activeBurnedSubtitle: Int?
    /// Read once, in `start`, rather than per open: a viewer who changes the
    /// setting from another device or another tab of Settings must not have the
    /// stream rebuilt under them mid-film. The choice a title started with is
    /// the choice it finishes with.
    private var subtitleReadiness: SubtitleReadiness = .onDemand
    /// Sticky for this playback. Once a native text track has been asked for —
    /// by automatic selection at cold start, or by the viewer — the stream keeps
    /// its subtitle renditions, including after subtitles are turned off again:
    /// dropping back to direct play would be a second restart nobody asked for,
    /// and the next selection would only have to pay for a third.
    private var wantsNativeSubtitleRenditions = false
    /// True while the player is on the raw file instead of an HLS session.
    /// P2-7: the first native subtitle selection has to create the session.
    private var isDirectPlayback = false
    /// Holds the newest seek/track intent that arrived mid-change so it wins
    /// instead of vanishing.
    private var reopenQueue = PlayerReopenQueue()
    /// Optimistic absolute film position for an interactive seek. It is also
    /// the base for the next relative press until the newest seek lands.
    private var seekState = PlayerSeekState()
    private var playbackRecoveryMonitor = PlaybackRecoveryMonitor()
    /// A clock freeze without an AVPlayerItem failure is not proof that the
    /// decoder rejected the stream. The same is true of a sustained buffering
    /// wait. Reconnect the exact delivery once; if the replacement freezes too,
    /// stop visibly instead of walking the HDR/SDR compatibility ladder on a
    /// guess.
    private var sameDeliveryStallRecovery = SameDeliveryStallRecoveryState()
    /// Evidence that this server understands `native_subtitles`: its create
    /// response handed back a native master query. A server predating the
    /// feature returns the plain playlist URL and advertises no subtitle
    /// group, and only that combination may use the legacy burn fallback.
    private var serverServesNativeSubtitles = false
    /// Set only once a server has proved it predates `native_subtitles`: its
    /// text tracks then go back through the pre-branch burn path.
    private var forceLegacySubtitleBurn = false
    /// Identifies the newest `open()`. An older attempt that wakes from its
    /// awaits afterwards must not replace the item, clear the transition
    /// state, or report its own failure over the newer one's (P2-6).
    private var openGeneration = 0
    /// The transport the viewer asked for, which is not what AVPlayer reports
    /// while it buffers or after an item fails. Reopens restore this.
    private var wantsPlayback = true
    /// The last rate the player was genuinely playing at, so a viewer paused
    /// at 1.5× resumes at 1.5× rather than at the 0 the transport reports
    /// while paused (P2-5).
    private var preferredRate: Float = 1
    /// The viewer's audio language, kept because this controller now performs
    /// media selection itself instead of leaving it to AVPlayer criteria.
    private var audioLanguage = "eng"
    /// Stable for this player instance. Server-side supersession uses it to
    /// replace this player's own stream without touching another device.
    private let playbackId = UUID().uuidString
    private var pgsOverlayTrackIndex: Int?
    private var pgsOverlayManifest: PGSOverlayManifest?
    private var pgsOverlayPrepareTask: Task<Void, Never>?
    private var pgsOverlayWindowTask: Task<Void, Never>?
    private var pgsOverlaySelectionGeneration = 0
    private var pgsOverlayItemGeneration = 0
    private var pgsOverlayRevision = 0
    private var pgsOverlayImageCache: [String: CGImage] = [:]
    private var pgsOverlayImageBytes: [String: Int] = [:]
    private var pgsOverlayImageLRU: [String] = []

    #if os(iOS)
    private var remoteTargets: [(MPRemoteCommand, Any)] = []
    #endif

    var subtitles: [SubtitleTrack] { decision?.subtitles ?? [] }
    var audioTracks: [AudioTrack] { decision?.audio ?? [] }
    var qualityRungs: [QualityRung] { decision?.ladder ?? [] }
    var activeMarker: Marker? {
        decision?.markers?.first { currentMs >= $0.startMs && currentMs < $0.endMs }
    }

    var methodLabel: String {
        #if os(iOS)
        if offlineId != nil {
            let quality = selectedHeight.map { " · \($0)p" } ?? ""
            return "Offline\(quality) · H.264 · AAC"
        }
        #endif
        if activeBurnedSubtitle != nil { return "Transcode · subtitle burn-in" }
        if selectedHeight != nil { return "Transcode · \(selectedHeight!)p" }
        let mode = decision.map(Self.playbackMode) ?? "transcode"
        let overlay = pgsOverlayTrackIndex == selectedSubtitle ? " · PGS overlay" : ""
        switch mode {
        case "direct": return "Direct play\(overlay)"
        case "remux": return "Remux · HLS\(overlay)"
        default: return (isVOD ? "Transcode · cached" : "Transcode") + overlay
        }
    }

    private var clientLogMethod: String {
        isDirectPlayback ? "direct_play" : (encoder == "copy" ? "remux" : "transcode")
    }

    var pgsOverlayIsActive: Bool { pgsOverlayTrackIndex == selectedSubtitle }

    var observedBitrate: Double? {
        player.currentItem?.accessLog()?.events.last?.observedBitrate
    }

    var indicatedBitrate: Double? {
        player.currentItem?.accessLog()?.events.last?.indicatedBitrate
    }

    var stalls: Int? {
        player.currentItem?.accessLog()?.events.last?.numberOfStalls
    }

    var presentationSize: CGSize { player.currentItem?.presentationSize ?? .zero }

    func start(
        model: AppModel,
        itemId: Int,
        fileId: Int,
        startMs: Int,
        durationMs: Int,
        title: String
    ) {
        guard !started else { return }
        started = true
        self.model = model
        self.itemId = itemId
        self.fileId = fileId
        self.knownDurationMs = durationMs
        self.currentMs = max(0, startMs)
        self.title = title
        subtitleReadiness = model.subtitleReadiness
        wantsNativeSubtitleRenditions = false
        canRetryCurrentItemWithHDRBase = false
        dolbyVisionFallbackAttempted = false
        forceCompatibleHDRBase = false
        canRetryCurrentItemWithTranscode = false
        compatibilityFallbackAttempted = false
        forceCompatibilityTranscode = false
        establishedPlayback = false
        establishedHDRRetryAttempted = false
        sameDeliveryStallRecovery.reset()
        attachedAtPositionMs = max(0, startMs)
        clearPlaybackNotice()

        #if os(iOS)
        // iOS needs an explicit playback audio session for silent-switch and
        // background/PiP behavior. Activating it on tvOS was a regression:
        // Apple TV owns the output route and AVPlayer can remain waiting even
        // though the item and server are healthy.
        try? AVAudioSession.sharedInstance().setCategory(.playback)
        try? AVAudioSession.sharedInstance().setActive(true)
        installRemoteCommands()
        #endif

        audioLanguage = model.audioLang
        // P2-8, taking the plan's second option: own media selection outright.
        // Leaving automatic criteria on lets AVPlayer enable a legible
        // rendition from the master's AUTOSELECT/DEFAULT metadata and the
        // system caption preference as soon as the item loads — before the
        // explicit selection lands — which puts a text track on top of a
        // burned-in one for the first seconds of a burn session. Emptying the
        // legible criteria is not enough, because those two sources remain.
        // The plan's first option (apply the selection before `play()`) was
        // rejected: the selection needs an await, and gating the first
        // `play()` behind item readiness is the tvOS transport regression
        // recorded in `open`. Owning selection means owning audio too, which
        // `applyPreferredAudioSelection` does per item.
        player.appliesMediaSelectionCriteriaAutomatically = false
        player.automaticallyWaitsToMinimizeStalling = true
        addPeriodicObserver()
        startPlaybackRecoveryMonitor()

        Task { await load(startMs: startMs) }
    }

    #if os(iOS)
    /// Attach a system-managed local HLS package. This branch deliberately
    /// constructs its decision-shaped snapshot locally and performs no API or
    /// capability-URL work: airplane-mode playback is a first-class source,
    /// not a remote failure fallback.
    func startOffline(model: AppModel, item offline: OfflineItem) {
        guard !started, let path = offline.localAssetRelativePath else { return }
        started = true
        self.model = model
        offlineId = offline.id
        itemId = offline.itemId
        fileId = offline.fileId
        knownDurationMs = offline.durationMs ?? 0
        currentMs = max(0, offline.positionMs)
        title = offline.title
        selectedHeight = offline.actualHeight
        selectedAudio = offline.audioLabel == nil ? nil : 0
        selectedSubtitle = offline.subtitleIndex == nil ? nil : 0
        deliveredRange = "sdr"
        encoder = "offline"
        isVOD = true
        usesDirectTimeline = true
        isDirectPlayback = true
        decision = Self.offlineDecision(offline)
        wantsPlayback = true
        player.appliesMediaSelectionCriteriaAutomatically = false
        player.automaticallyWaitsToMinimizeStalling = true
        try? AVAudioSession.sharedInstance().setCategory(.playback)
        try? AVAudioSession.sharedInstance().setActive(true)
        installRemoteCommands()
        addPeriodicObserver()
        Task { await loadOffline(url: OfflineCatalog.localURL(for: path), startMs: currentMs) }
    }

    private static func offlineDecision(_ item: OfflineItem) -> Decision {
        let audio = item.audioLabel.map {
            [AudioTrack(index: 0, codec: "aac", channels: nil, language: nil, title: $0, default: true)]
        }
        let subtitles = item.subtitleIndex.map { _ in
            [SubtitleTrack(
                index: 0,
                codec: "webvtt",
                language: nil,
                title: item.subtitleLabel ?? "Downloaded",
                default: true,
                forced: false,
                text: true,
                native: true,
                overlay: nil
            )]
        }
        return Decision(
            fileId: item.fileId,
            method: "offline",
            playUrl: "",
            delivery: Delivery(mode: "direct", url: nil, sessionsUrl: nil, aac: nil, preserveDolbyVision: nil),
            reasons: ["Downloaded for offline viewing"],
            transcodeAudio: false,
            preserveDolbyVision: false,
            source: SourceSummary(
                container: "mpegts",
                videoCodec: "h264",
                videoProfile: nil,
                width: nil,
                height: item.actualHeight,
                bitDepth: 8,
                hdr: nil,
                hdrFormat: nil,
                bitrate: nil,
                durationMs: item.durationMs
            ),
            audio: audio,
            subtitles: subtitles,
            markers: item.markers,
            audioOffsetMs: 0,
            declaredOffsetMs: 0,
            ladder: [],
            deliveredDynamicRange: "sdr"
        )
    }

    private func loadOffline(url: URL, startMs: Int) async {
        let asset = AVURLAsset(url: url)
        guard asset.assetCache?.isPlayableOffline == true else {
            fail(APIError.transport("Download incomplete"))
            return
        }
        let item = AVPlayerItem(asset: asset)
        Self.configureBuffering(item, growingHLS: false)
        item.externalMetadata = [titleMetadata(title)]
        observeEnd(of: item)
        observeStatus(of: item)
        player.replaceCurrentItem(with: item)
        baseMs = 0
        player.play()
        if startMs > 0 {
            do { try await seekWhenReady(item, ms: startMs) }
            catch { fail(error); return }
        }
        await applyPreferredAudioSelection(to: item)
        await applyNativeSubtitleSelection(selectedSubtitle, to: item)
        player.play()
        isPlaying = true
        failed = false
        attachedAtPositionMs = startMs
        updateNowPlaying()
    }
    #endif

    func togglePlayPause() {
        // A buffering player reports `.waitingToPlayAtSpecifiedRate` and rate
        // zero even though Play is still the viewer's intent. Keying this
        // control from the intent prevents a wait from masquerading as a user
        // pause (and from turning a pause press into another play request).
        if wantsPlayback {
            player.pause()
            isPlaying = false
            wantsPlayback = false
        } else {
            player.play()
            if preferredRate != 1 { player.rate = preferredRate }
            isPlaying = true
            wantsPlayback = true
        }
        updateNowPlaying()
    }

    func skip(seconds: Double) {
        let request = seekState.relative(
            by: Int(seconds * 1000),
            observedMs: positionForPlaybackIntent(),
            durationMs: knownDurationMs
        )
        issueSeek(to: request.target, generation: request.generation)
    }

    func skipActiveMarker() {
        guard let marker = activeMarker else { return }
        seek(toMs: marker.endMs)
    }

    func seek(toMs requested: Int) {
        let request = seekState.absolute(requested, durationMs: knownDurationMs)
        issueSeek(to: request.target, generation: request.generation)
    }

    private func issueSeek(to target: Int, generation: Int) {
        // Move the visible timeline immediately. The old implementation left
        // it on the paused predecessor for the whole server round trip, which
        // made tvOS look as though the progress command had not worked.
        currentMs = target
        refreshPGSOverlayWindow(at: target, force: true)
        playbackRecoveryMonitor.reset()
        let requiresReopen = isChangingStream || !(usesDirectTimeline || isVOD)
        Task {
            if !requiresReopen {
                _ = await player.seek(
                    to: CMTime(seconds: Double(target) / 1000.0, preferredTimescale: 600),
                    toleranceBefore: .zero,
                    toleranceAfter: .zero
                )
                guard seekState.complete(generation: generation) else { return }
                currentMs = realPositionMs()
                updateNowPlaying()
            } else {
                await reopen(at: target)
            }
        }
    }

    func selectSubtitle(_ index: Int?) {
        guard index != selectedSubtitle else { return }
        if Self.subtitleUsesOverlay(index, in: subtitles), player.isExternalPlaybackActive {
            showPlaybackNotice(Self.pgsOverlayExternalPlaybackNotice)
            return
        }
        if Self.subtitleBurnWouldDiscardHDR(
            index,
            tracks: subtitles,
            deliveredRange: deliveredRange
        ) {
            showPlaybackNotice(Self.hdrSubtitleNotice)
            return
        }
        clearPlaybackNotice()
        let activeOverlay = pgsOverlayTrackIndex
        selectedSubtitle = index
        let route = Self.subtitleSelectionRoute(
            for: index,
            tracks: subtitles,
            activeBurn: activeBurnedSubtitle,
            isDirectPlayback: isDirectPlayback,
            activeOverlay: activeOverlay
        )
        updatePGSOverlaySelection(index)
        // Set before the reopen is scheduled: the open it leads to reads this
        // to decide it may no longer direct-play, and it stays set for the rest
        // of the title, so turning subtitles off again costs no second restart.
        if let index, !Self.subtitleRequiresBurn(index, in: subtitles) {
            wantsNativeSubtitleRenditions = true
        }
        Task { await applySubtitleSelection(index, route: route) }
    }

    /// Nonfatal playback feedback shares the red player banner with recovery
    /// messages, but not their lifetime. Replacing the notice restarts its
    /// clock; real playback failures continue through `playbackError` and the
    /// persistent failure view.
    func showPlaybackNotice(_ message: String, duration: Duration = .seconds(5)) {
        playbackNoticeTask?.cancel()
        playbackNotice = message
        playbackNoticeTask = Task { [weak self] in
            do {
                try await Task.sleep(for: duration)
            } catch {
                return
            }
            guard let self, self.playbackNotice == message else { return }
            self.playbackNotice = nil
            self.playbackNoticeTask = nil
        }
    }

    func clearPlaybackNotice() {
        playbackNoticeTask?.cancel()
        playbackNoticeTask = nil
        playbackNotice = nil
    }

    private func applySubtitleSelection(_ index: Int?, route: SubtitleSelectionRoute) async {
        switch route {
        case .reopen:
            await reopen(at: positionForPlaybackIntent())
        case .bitmapOverlay:
            // Moving from a native rendition to bitmap presentation must also
            // turn AVPlayer's legible option off. The synchronized layer then
            // owns the only subtitle pixels without replacing the video item.
            await applyNativeSubtitleSelection(nil, to: player.currentItem)
        case .mediaSelection:
            // Selection belongs to AVPlayerItem, not the HLS session. This is
            // the no-restart path that preserves video copy, HDR, position,
            // and the viewer's selected quality.
            await applyNativeSubtitleSelection(index, to: player.currentItem)
        }
    }

    func selectAudio(_ index: Int) {
        guard index != selectedAudio else { return }
        selectedAudio = index
        audioOverride = index
        Task { await reopen(at: positionForPlaybackIntent()) }
    }

    func selectQuality(_ height: Int?) {
        guard height != selectedHeight else { return }
        selectedHeight = height
        Task { await reopen(at: positionForPlaybackIntent()) }
    }

    /// Film position that a new viewer command should build on. Between
    /// commands the live AVPlayer clock is most precise; during a seek or
    /// replacement, only the optimistic target / last published film time is
    /// in the right timeline.
    private func positionForPlaybackIntent() -> Int {
        if let pending = seekState.pendingMs { return pending }
        return isChangingStream ? currentMs : realPositionMs()
    }

    /// Report the final position and hand any encoder back immediately.
    func stop() {
        clearPlaybackNotice()
        guard started else { return }
        started = false
        if let timeObserver {
            player.removeTimeObserver(timeObserver)
            self.timeObserver = nil
        }
        if let endObserver {
            NotificationCenter.default.removeObserver(endObserver)
            self.endObserver = nil
        }
        itemStatusObservation = nil
        statusTask?.cancel()
        statusTask = nil
        recoveryTask?.cancel()
        recoveryTask = nil
        clearPGSOverlaySelection()
        playbackRecoveryMonitor.reset()
        seekState.clear()
        let position = realPositionMs()
        player.pause()
        isPlaying = false
        report(position)
        if let sessionId {
            self.sessionId = nil
            let model = model
            Task { await model?.endHlsSession(sessionId) }
        }
        #if os(iOS)
        removeRemoteCommands()
        MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
        MPNowPlayingInfoCenter.default().playbackState = .stopped
        #endif
    }

    /// True film position, including an HLS session's server-side start base.
    func realPositionMs() -> Int {
        let seconds = player.currentTime().seconds
        let local = seconds.isFinite ? Int(seconds * 1000) : 0
        return baseMs + max(local, 0)
    }

    // MARK: - Stream lifecycle

    private func load(startMs: Int) async {
        guard let model else { return }
        do {
            let decision = try await model.decision(fileId: fileId)
            self.decision = decision
            if knownDurationMs <= 0 { knownDurationMs = decision.source?.durationMs ?? 0 }
            // `default` on a decision track is the server's own shared-policy
            // pick, not the muxer's flag (crates/plurxd http/stream.rs
            // overwrites both lists from `select_tracks`).
            selectedAudio = decision.audio?.first(where: { $0.default })?.index
            // "Off" in this device's settings is the viewer saying never, so
            // it is honored before the server's pick is even looked at. It is
            // the viewer's instruction rather than a second copy of the
            // selection rule, which lives on the server alone.
            let automaticSubtitle = model.subLang == "off"
                ? nil
                : Self.automaticSubtitleIndex(decision.subtitles ?? [])
            // A forced bitmap track may be the server's automatic pick, but a
            // cold-start preference still must not trade an HDR plan for an
            // SDR burn without the viewer asking. Manual selection is guarded
            // by the same predicate in `selectSubtitle`.
            selectedSubtitle = Self.subtitleBurnWouldDiscardHDR(
                automaticSubtitle,
                tracks: decision.subtitles ?? [],
                deliveredRange: decision.deliveredDynamicRange
            ) ? nil : automaticSubtitle
            updatePGSOverlaySelection(selectedSubtitle)
            // Use the same drain as later replacements. A remote command can
            // arrive while the decision request or first item is preparing;
            // opening directly here left that command stranded in the reopen
            // queue forever. A command issued before the decision arrived is
            // already represented by `seekState.pendingMs`.
            let initialPosition = seekState.pendingMs ?? startMs
            try await openAndDrain(decision: decision, at: initialPosition)
        } catch {
            reopenQueue.clear()
            seekState.clear()
            fail(error)
        }
    }

    private func reopen(at position: Int) async {
        // A growing EVENT playlist can momentarily announce its current end,
        // and several UI actions can also request a restart. Never overlap two
        // server-session replacements (see `PlayerReopenQueue`) — they share a
        // playback ID, so the newer request intentionally removes the older
        // session and can otherwise strand AVPlayer on a URL the server has
        // just deleted — but never discard one either: a request that lands
        // mid-change is remembered and replayed as the single trailing reopen.
        guard let decision, started else { return }
        guard let next = reopenQueue.request(position, changeInFlight: isChangingStream) else {
            return
        }
        do {
            try await openAndDrain(decision: decision, at: next)
        } catch {
            reopenQueue.clear()
            seekState.clear()
            currentMs = realPositionMs()
            fail(error)
        }
    }

    /// Open, then honor whatever queued behind that open. A loop rather than
    /// recursion, so "exactly one trailing reopen per completed change" stays
    /// true however long a burst of step-seeks runs. Nothing awaits between a
    /// finished `open` and the next one, so no other request can observe the
    /// gap and start a competing replacement.
    private func openAndDrain(decision: Decision, at startMs: Int) async throws {
        var next = startMs
        while true {
            try await open(decision: decision, at: next)
            guard started else {
                reopenQueue.clear()
                return
            }
            // A newer `open()` superseded this one while it ran (P2-6). The
            // transition — and with it `isChangingStream`, which only the
            // owning open clears — belongs to that newer attempt, so the queued
            // request is its to drain and not this loop's.
            guard !isChangingStream else { return }
            guard let trailing = reopenQueue.takePending() else {
                seekState.completeReopen(at: next)
                return
            }
            next = trailing
        }
    }

    private func open(decision: Decision, at startMs: Int) async throws {
        guard let model, started else { return }
        openGeneration &+= 1
        let generation = openGeneration
        isChangingStream = true
        failed = false
        playbackError = nil
        // P2-5: a reopen must not un-pause a viewer who paused before changing
        // audio, quality, or a burned subtitle — and must not pause one whose
        // player merely happens to be stopped right now, which is the state a
        // buffering item and a failed item both present.
        let resumesPlayback = Self.reopenResumesPlayback(
            wantsPlayback: wantsPlayback,
            hasCurrentItem: player.currentItem != nil
        )
        let resumeRate = preferredRate
        // What `restoreAfterFailedChange` has to put back if no successor comes
        // into existence: a stream that is still worth watching, and the
        // viewer's own transport intent rather than the player's observed
        // state, which reads as paused while an item buffers (P2-5).
        let wasPlaying = wantsPlayback && player.currentItem != nil
        player.pause()
        // The session this open replaces. It is retired only once its successor
        // exists: releasing first meant a failed create left the viewer's item
        // pointing at a playlist this client had just deleted — buffered
        // runway, then a stall, with `fail()` deliberately quiet because an
        // item still existed. Its telemetry poll stops now; the session itself
        // does not.
        let superseded = sessionId
        stopStatusPolling()

        let normalMode = Self.playbackMode(decision)
        let preserveDolbyVision = Self.shouldPreserveDolbyVision(decision)
            && !forceCompatibleHDRBase
        // Captured once: `selectedSubtitle` may change while the awaits below
        // run, and the difference is reconciled when this open completes (P1-2).
        let requestedSubtitle = selectedSubtitle
        let subtitleFields = Self.sessionSubtitleFields(
            selected: requestedSubtitle,
            tracks: subtitles,
            legacyBurn: forceLegacySubtitleBurn
        )
        let burnSubtitle = subtitleFields.burn
        let nativeSubtitle = subtitleFields.native
        let forceTranscode = burnSubtitle != nil || selectedHeight != nil || forceCompatibilityTranscode
        let customAudio = audioOverride != nil
        // Whether this open has to be a session for no reason other than making
        // the file's text subtitles selectable. Sticky once true, so leaving
        // subtitles again does not buy a second restart.
        let needsSubtitleRenditions = Self.needsNativeSubtitleSession(
            hasNativeTextTrack: subtitles.contains(where: \.isNativeHLS),
            readiness: subtitleReadiness,
            subtitlesInUse: wantsNativeSubtitleRenditions
        )
        canRetryCurrentItemWithTranscode = normalMode != "transcode" && !forceTranscode && !customAudio
        canRetryCurrentItemWithHDRBase = false
        // P2-7, decided by Paul on 2026-08-02 (plan §2.5): stay direct until
        // the first native selection. Merely *having* native text tracks no
        // longer abolishes true direct play — every such file used to become a
        // copy session, and on Bedroom that path degrades to a compatibility
        // transcode. Entering the session is deferred to the moment a native
        // subtitle is actually chosen, which costs exactly one reopen there
        // (routed explicitly by `subtitleSelectionRoute`).
        let direct = normalMode == "direct" && !forceTranscode && !customAudio
            && nativeSubtitle == nil && !needsSubtitleRenditions
        let url: URL?
        var seekAfterAttach: Int?
        var nextBaseMs = 0

        if direct {
            activeBurnedSubtitle = nil
            isDirectPlayback = true
            nextBaseMs = 0
            usesDirectTimeline = true
            isVOD = true
            encoder = nil
            // Direct play has no session, so the decision's answer stands for
            // the whole playback (MEDIA-BADGES-PLAN.md §3.2).
            deliveredRange = decision.deliveredDynamicRange
            url = Session.shared.mediaURL(decision.delivery?.url ?? decision.playUrl)
            if startMs > 0 { seekAfterAttach = startMs }
        } else {
            let copy = !forceTranscode
                && (normalMode == "direct" || normalMode == "remux" || customAudio)
            canRetryCurrentItemWithHDRBase = copy
                && preserveDolbyVision
                && Self.hasCompatibleDolbyVisionBase(decision.source?.hdrFormat)
            // The checkmark comes from `/decision`'s shared-policy pick. Carry
            // that exact track into the initial session too: relying on an
            // omitted audio field made copy-video HLS fall back to the muxer's
            // first stream, so the UI could say English while Italian played.
            // A viewer's later explicit choice remains the stronger value.
            let chosenAudio = Self.sessionAudioIndex(
                explicit: audioOverride,
                selected: selectedAudio
            )
            let aac = copy ? needsAAC(audioIndex: chosenAudio, decision: decision)
                : nil
            let body = CreateSessionRequest(
                playbackId: playbackId,
                height: Self.burnSessionHeight(
                    burnSubtitle: burnSubtitle,
                    mode: normalMode,
                    selectedHeight: selectedHeight,
                    sourceHeight: decision.source?.height
                ),
                start: Double(startMs) / 1000.0,
                audio: chosenAudio,
                subtitleBurn: burnSubtitle,
                nativeSubtitles: true,
                subtitle: nativeSubtitle,
                copy: copy ? true : nil,
                aac: copy ? aac : nil,
                preserveDolbyVision: copy ? preserveDolbyVision : nil
            )
            let hls: HlsStart
            do {
                hls = try await model.createHlsSession(fileId: fileId, body: body)
            } catch {
                // A superseded attempt must not report its own failure over
                // the newer open's state (P2-6), and must not put back a stream
                // the newer open has already taken ownership of.
                if isSuperseded(generation) { return }
                // Nothing came into existence to replace the current stream, so
                // nothing about it changes: same session, same player item,
                // telemetry running again, and playing if it was. The caller
                // surfaces a transient error instead of a coming stall.
                restoreAfterFailedChange(wasPlaying: wasPlaying, session: superseded)
                throw error
            }
            guard !isSuperseded(generation) else {
                await model.endHlsSession(hls.sessionId)
                return
            }
            guard started else {
                await model.endHlsSession(hls.sessionId)
                isChangingStream = false
                return
            }
            sessionId = hls.sessionId
            isDirectPlayback = false
            serverServesNativeSubtitles = Self.playlistAdvertisesNativeSubtitles(hls.playlistUrl)
            activeBurnedSubtitle = burnSubtitle
            encoder = hls.encoder
            // The session that exists overrides the plan that was decided: a
            // burn or an explicitly picked rung forces a transcode the decision
            // never promised. A server that cannot answer leaves the decision's
            // value standing rather than blanking the badge. Set only after a
            // successful create, so a failed change leaves the still-playing
            // stream's answer alone.
            deliveredRange = hls.deliveredDynamicRange ?? decision.deliveredDynamicRange
            isVOD = hls.vod ?? false
            usesDirectTimeline = isVOD
            if knownDurationMs <= 0 { knownDurationMs = hls.durationMs ?? 0 }
            nextBaseMs = Self.sessionMediaOriginMs(hls, requestedStartMs: startMs)
            if isVOD {
                if startMs > 0 { seekAfterAttach = startMs }
            }
            url = Session.shared.url(hls.playlistUrl)
            startStatusPolling()
        }

        guard let url else {
            restoreAfterFailedChange(wasPlaying: wasPlaying, session: superseded)
            throw APIError.badURL
        }
        // Nothing backs a direct play, and `superseded` still names the session
        // being replaced — so it is safe to forget here and released below.
        if direct { sessionId = nil }
        // The successor exists — only now does the predecessor go. Server-side
        // supersession has already retired it (the create carried the same
        // `playback_id`); this DELETE just hands the encoder slot back at once
        // rather than at the idle reaper's convenience.
        await release(session: superseded)
        guard !isSuperseded(generation) else { return }
        // That DELETE is a network round trip, so the viewer can leave during
        // it. Attaching an item and calling `play()` after `stop()` has run
        // would resurrect a player nobody is watching — and with the background
        // audio mode on, keep it audible. `stop()` has already retired whatever
        // session this open created.
        guard started else {
            isChangingStream = false
            return
        }
        let item = AVPlayerItem(url: url)
        Self.configureBuffering(item, growingHLS: sessionId != nil && !isVOD)
        #if os(iOS)
        // The tvOS 26 SDK exposes this setter, but some shipping Apple TV
        // runtimes do not implement it and abort on the Objective-C selector.
        // iOS uses it for system presentation metadata; tvOS uses our custom
        // player chrome and does not need item-level external metadata.
        item.externalMetadata = [titleMetadata(title)]
        #endif
        observeEnd(of: item)
        observeStatus(of: item)
        pgsOverlayItemGeneration &+= 1
        pgsOverlayWindowTask?.cancel()
        pgsOverlayWindow = nil
        player.replaceCurrentItem(with: item)
        // Publish the new local-to-film mapping only once the new item is the
        // one whose clock `realPositionMs()` reads. Updating it during session
        // creation mixed the predecessor's local time into the successor's
        // base and was the source of the apparently random seek jumps.
        baseMs = nextBaseMs
        refreshPGSOverlayWindow(at: startMs, force: true)
        // Start loading/playing immediately. Previously a resume point gated
        // this call behind item readiness and could leave tvOS permanently
        // presenting a stopped transport. That regression is why a paused
        // reopen still calls `play()` here rather than betting that an item
        // reaches `.readyToPlay` at rate 0 on every shipping tvOS: prepare
        // first, honor the pause immediately when nothing has to be waited
        // for, and otherwise as soon as the resume seek lands.
        player.play()
        if !resumesPlayback && seekAfterAttach == nil { player.pause() }
        isPlaying = resumesPlayback
        if let seekAfterAttach {
            do {
                try await seekWhenReady(item, ms: seekAfterAttach)
            } catch {
                if isSuperseded(generation) { return }
                throw error
            }
        }
        await applyPreferredAudioSelection(to: item)
        await applyNativeSubtitleSelection(nativeSubtitle, to: item)
        guard !isSuperseded(generation) else { return }
        if resumesPlayback {
            player.play()
            // Restore the rate the viewer was last actually playing at (P2-5).
            if resumeRate != 1 { player.rate = resumeRate }
        } else {
            player.pause()
        }
        isPlaying = resumesPlayback
        currentMs = startMs
        attachedAtPositionMs = startMs
        playbackRecoveryMonitor.reset()
        failed = false
        isChangingStream = false
        updateNowPlaying()
        // P1-2: `reopen()` queues rather than overlaps an in-flight open, and
        // this open applied the selection it captured at entry, so a track
        // picked during a cold extraction would otherwise show a checkmark
        // forever while the stream renders the old choice — a media-selection
        // switch queues nothing, because it never asked for a reopen. Apply
        // whatever the viewer last chose.
        if let route = Self.subtitleReconciliation(
            applied: requestedSubtitle,
            current: selectedSubtitle,
            tracks: subtitles,
            activeBurn: activeBurnedSubtitle,
            isDirectPlayback: isDirectPlayback
        ) {
            await applySubtitleSelection(selectedSubtitle, route: route)
        }
    }

    /// True when a newer `open()` has taken ownership of the player. The older
    /// attempt then returns without replacing the item, clearing
    /// `isChangingStream`, or reporting its own failure (P2-6).
    private func isSuperseded(_ generation: Int) -> Bool { generation != openGeneration }

    /// Stop polling the session being left behind. The session itself survives
    /// this call — it is retired by `release(session:)` once its replacement
    /// exists.
    private func stopStatusPolling() {
        statusTask?.cancel()
        statusTask = nil
        sessionStatus = nil
    }

    /// Retire a superseded HLS session, best effort.
    private func release(session id: String?) async {
        guard let id else { return }
        await model?.endHlsSession(id)
    }

    /// Put back everything `open` disturbed before it discovered it could not
    /// produce a successor. The viewer keeps watching what they were watching.
    private func restoreAfterFailedChange(wasPlaying: Bool, session: String?) {
        isChangingStream = false
        // The create it failed on is a round trip too: if the viewer left
        // during it there is nothing left to keep watching, and resuming here
        // would restart a player `stop()` has already paused and detached from.
        guard started else { return }
        if session != nil { startStatusPolling() }
        if wasPlaying {
            player.play()
            // Restore the rate the viewer was last actually playing at (P2-5).
            if preferredRate != 1 { player.rate = preferredRate }
            isPlaying = true
        }
    }

    // MARK: - PGS application overlay

    /// Custom application layers are not carried by Apple's system PiP or
    /// external playback surfaces. Refuse that output while PGS is selected;
    /// never replace the current HDR/Dolby Vision bytes with a hidden burn.
    func allowsPictureInPictureCommand() -> Bool {
        guard !pgsOverlayIsActive else {
            showPlaybackNotice(Self.pgsOverlayExternalPlaybackNotice)
            return false
        }
        return true
    }

    private func updatePGSOverlaySelection(_ index: Int?) {
        guard let index, Self.subtitleUsesOverlay(index, in: subtitles) else {
            clearPGSOverlaySelection()
            return
        }
        guard pgsOverlayTrackIndex != index else {
            refreshPGSOverlayWindow(at: positionForPlaybackIntent())
            return
        }

        clearPGSOverlaySelection()
        pgsOverlayTrackIndex = index
        pgsOverlayStatus = .preparing
        // AirPlay/external display would carry only the video plane. Keep the
        // selection local until output-specific overlay behavior is proven.
        player.allowsExternalPlayback = false
        pgsOverlaySelectionGeneration &+= 1
        let selectionGeneration = pgsOverlaySelectionGeneration

        pgsOverlayPrepareTask = Task { [weak self] in
            guard let self, let model = self.model else { return }
            do {
                let clock = ContinuousClock()
                let deadline = clock.now.advanced(
                    by: .seconds(PGSOverlayPolicy.maximumPrepareSeconds)
                )
                while clock.now < deadline {
                    try Task.checkCancellation()
                    guard self.pgsOverlaySelectionGeneration == selectionGeneration,
                          self.pgsOverlayTrackIndex == index,
                          self.selectedSubtitle == index
                    else { return }
                    switch try await model.pgsOverlayManifest(
                        fileId: self.fileId,
                        trackIndex: index
                    ) {
                    case .ready(let rawManifest):
                        let manifest = try rawManifest.validated(
                            fileId: self.fileId,
                            trackIndex: index
                        )
                        guard self.pgsOverlaySelectionGeneration == selectionGeneration else {
                            return
                        }
                        self.pgsOverlayManifest = manifest
                        self.refreshPGSOverlayWindow(
                            at: self.positionForPlaybackIntent(),
                            force: true
                        )
                        return
                    case .preparing(let retryAfterMs):
                        try await Task.sleep(for: .milliseconds(retryAfterMs))
                    }
                }
                throw PGSOverlayError.preparationTimedOut
            } catch is CancellationError {
                return
            } catch {
                guard self.pgsOverlaySelectionGeneration == selectionGeneration else { return }
                self.pgsOverlayStatus = .failed(error.localizedDescription)
                self.pgsOverlayWindow = nil
                self.showPlaybackNotice(
                    "\(error.localizedDescription) Video playback was kept unchanged."
                )
            }
        }
    }

    private func clearPGSOverlaySelection() {
        pgsOverlaySelectionGeneration &+= 1
        pgsOverlayPrepareTask?.cancel()
        pgsOverlayPrepareTask = nil
        pgsOverlayWindowTask?.cancel()
        pgsOverlayWindowTask = nil
        pgsOverlayTrackIndex = nil
        pgsOverlayManifest = nil
        pgsOverlayWindow = nil
        pgsOverlayStatus = .off
        pgsOverlayImageCache.removeAll(keepingCapacity: false)
        pgsOverlayImageBytes.removeAll(keepingCapacity: false)
        pgsOverlayImageLRU.removeAll(keepingCapacity: false)
        player.allowsExternalPlayback = true
    }

    private func refreshPGSOverlayWindow(at sourceTimeMs: Int, force: Bool = false) {
        guard let manifest = pgsOverlayManifest,
              let trackIndex = pgsOverlayTrackIndex,
              selectedSubtitle == trackIndex,
              force || PGSOverlayPolicy.shouldRefresh(
                sourceTimeMs: sourceTimeMs,
                loadedRange: pgsOverlayWindow?.sourceRange
              )
        else { return }

        let sourceRange = PGSOverlayPolicy.windowRange(
            at: sourceTimeMs,
            durationMs: manifest.durationMs
        )
        let cues = Array(manifest.cues.lazy.filter {
            $0.endMs > sourceRange.lowerBound && $0.startMs < sourceRange.upperBound
        }.prefix(PGSOverlayPolicy.maximumScheduledCues))
        let selectionGeneration = pgsOverlaySelectionGeneration
        let itemGeneration = pgsOverlayItemGeneration
        let itemBaseMs = baseMs
        let generation = manifest.generation
        pgsOverlayWindowTask?.cancel()

        pgsOverlayWindowTask = Task { [weak self] in
            guard let self, let model = self.model else { return }
            do {
                guard PGSOverlayPolicy.windowFitsDecodedBudget(cues) else {
                    throw PGSOverlayError.memoryLimit
                }
                var rendered: [PGSOverlayRenderableCue] = []
                rendered.reserveCapacity(cues.count)
                var decodedWindowBytes = 0
                var decodedWindowPaths: Set<String> = []
                for cue in cues {
                    try Task.checkCancellation()
                    var objects: [PGSOverlayRenderableObject] = []
                    objects.reserveCapacity(cue.objects.count)
                    for object in cue.objects {
                        try Task.checkCancellation()
                        let image: CGImage
                        if let cached = self.cachedPGSOverlayImage(object.image) {
                            image = cached
                        } else {
                            let data = try await model.pgsOverlayObject(
                                fileId: self.fileId,
                                trackIndex: trackIndex,
                                generation: generation,
                                path: object.image
                            )
                            guard let decoded = UIImage(data: data)?.cgImage,
                                  decoded.width == object.width,
                                  decoded.height == object.height
                            else { throw PGSOverlayError.invalidImage }
                            try self.storePGSOverlayImage(decoded, for: object.image)
                            image = decoded
                        }
                        if decodedWindowPaths.insert(object.image).inserted {
                            let (bytes, overflow) = image.bytesPerRow
                                .multipliedReportingOverflow(by: image.height)
                            guard !overflow,
                                  bytes > 0,
                                  bytes <= PGSOverlayPolicy.decodedImageBudgetBytes
                                    - decodedWindowBytes
                            else { throw PGSOverlayError.memoryLimit }
                            decodedWindowBytes += bytes
                        }
                        objects.append(PGSOverlayRenderableObject(object: object, image: image))
                    }
                    rendered.append(PGSOverlayRenderableCue(cue: cue, objects: objects))
                }

                guard self.pgsOverlaySelectionGeneration == selectionGeneration,
                      self.pgsOverlayItemGeneration == itemGeneration,
                      self.pgsOverlayTrackIndex == trackIndex,
                      self.selectedSubtitle == trackIndex
                else { return }
                self.pgsOverlayRevision &+= 1
                self.pgsOverlayWindow = PGSOverlayWindow(
                    revision: self.pgsOverlayRevision,
                    generation: generation,
                    baseMs: itemBaseMs,
                    sourceRange: sourceRange,
                    cues: rendered
                )
                self.pgsOverlayStatus = .ready
            } catch is CancellationError {
                return
            } catch {
                guard self.pgsOverlaySelectionGeneration == selectionGeneration else { return }
                self.pgsOverlayStatus = .failed(error.localizedDescription)
                self.pgsOverlayWindow = nil
                self.showPlaybackNotice(
                    "\(error.localizedDescription) Video playback was kept unchanged."
                )
            }
        }
    }

    private func cachedPGSOverlayImage(_ key: String) -> CGImage? {
        guard let image = pgsOverlayImageCache[key] else { return nil }
        pgsOverlayImageLRU.removeAll(where: { $0 == key })
        pgsOverlayImageLRU.append(key)
        return image
    }

    private func storePGSOverlayImage(_ image: CGImage, for key: String) throws {
        let (bytes, overflow) = image.bytesPerRow.multipliedReportingOverflow(
            by: image.height
        )
        guard !overflow,
              bytes > 0,
              bytes <= PGSOverlayPolicy.decodedImageBudgetBytes
        else {
            throw PGSOverlayError.memoryLimit
        }
        while pgsOverlayImageBytes.values.reduce(0, +) + bytes
            > PGSOverlayPolicy.decodedImageBudgetBytes,
              let oldest = pgsOverlayImageLRU.first {
            pgsOverlayImageLRU.removeFirst()
            pgsOverlayImageCache.removeValue(forKey: oldest)
            pgsOverlayImageBytes.removeValue(forKey: oldest)
        }
        guard pgsOverlayImageBytes.values.reduce(0, +) + bytes
                <= PGSOverlayPolicy.decodedImageBudgetBytes
        else { throw PGSOverlayError.memoryLimit }
        pgsOverlayImageCache[key] = image
        pgsOverlayImageBytes[key] = bytes
        pgsOverlayImageLRU.removeAll(where: { $0 == key })
        pgsOverlayImageLRU.append(key)
    }

    private func startStatusPolling() {
        statusTask?.cancel()
        statusTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, let sessionId = self.sessionId, let model = self.model else { return }
                self.sessionStatus = try? await model.hlsStatus(sessionId)
                try? await Task.sleep(nanoseconds: 2_000_000_000)
            }
        }
    }

    /// Sample the film clock independently of AVPlayer's periodic observer,
    /// which stops firing when that clock stops. Silent freezes and explicit
    /// buffering waits keep independent evidence: either may reconnect the
    /// exact delivery after a sustained lack of progress, but buffering can
    /// never enter the codec/HDR compatibility ladder. This restores the
    /// in-player equivalent of closing and reopening a title without reviving
    /// the false SDR fallbacks that originally caused buffering to be excluded.
    private func startPlaybackRecoveryMonitor() {
        recoveryTask?.cancel()
        recoveryTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                guard !Task.isCancelled, let self else { return }
                let shouldMonitor = self.started
                    && self.wantsPlayback
                    && !self.finished
                    && !self.failed
                    && !self.isChangingStream
                    && self.seekState.pendingMs == nil
                    && self.player.currentItem != nil
                let timeControlStatus = self.player.timeControlStatus
                let position = self.realPositionMs()
                guard let stallEvent = self.playbackRecoveryMonitor.sample(
                    positionMs: position,
                    timeControlStatus: timeControlStatus,
                    shouldMonitor: shouldMonitor,
                    // A first-frame wait may be the server deliberately
                    // filling its publish gate. Recovery begins only after
                    // this item has advanced for five seconds, which confines
                    // buffering recovery to the mid-playback failure reported
                    // on iPad and prevents duplicate cold-start sessions.
                    establishedPlayback: self.establishedPlayback
                ) else { continue }
                switch stallEvent.action {
                case .none:
                    continue
                case .nudge:
                    self.player.play()
                    if self.preferredRate != 1 { self.player.rate = self.preferredRate }
                    self.isPlaying = true
                case .reopen:
                    self.currentMs = position
                    if stallEvent.kind == .buffering {
                        await self.retrySameDeliveryAfterStall(stallEvent)
                        continue
                    }
                    if await self.retryEstablishedHDRDelivery(at: position) { continue }
                    await self.retrySameDeliveryAfterStall(stallEvent)
                }
            }
        }
    }

    /// One bounded transport recovery shared by buffering and non-HDR silent
    /// freezes. It deliberately calls `reopen` directly: no capability flag or
    /// selected format changes, so the replacement uses the identical recipe.
    private func retrySameDeliveryAfterStall(_ event: PlaybackStallEvent) async {
        let decision = sameDeliveryStallRecovery.next(for: event.kind)
        reportPlaybackStall(event, outcome: decision.outcome)
        switch decision {
        case .reopen:
            await reopen(at: event.positionMs)
        case .stop(let terminal):
            player.pause()
            isPlaying = terminal.isPlaying
            wantsPlayback = terminal.wantsPlayback
            isChangingStream = false
            failed = terminal.failed
            playbackError = terminal.message
        }
    }

    private func fail(_ error: Error) {
        isChangingStream = false
        failed = player.currentItem == nil || error is PlaybackPreparationError
        playbackError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }

    private func needsAAC(audioIndex: Int?, decision: Decision) -> Bool {
        guard let audioIndex,
              let codec = decision.audio?.first(where: { $0.index == audioIndex })?.codec.lowercased()
        else { return decision.delivery?.aac ?? decision.transcodeAudio ?? false }
        return !["aac", "ac3", "eac3", "alac", "mp3"].contains(codec)
    }

    // MARK: - Observation and metadata

    private func addPeriodicObserver() {
        let interval = CMTime(seconds: 1, preferredTimescale: 2)
        timeObserver = player.addPeriodicTimeObserver(forInterval: interval, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                // Keep an interactive target on screen while its item is being
                // prepared. Reading the predecessor here was the visible snap
                // back after a progress-bar or skip-button command.
                if self.seekState.pendingMs == nil && !self.isChangingStream {
                    self.currentMs = self.realPositionMs()
                }
                self.refreshPGSOverlayWindow(at: self.currentMs)
                // The last rate the viewer was genuinely playing at, so a
                // pause at 1.5× is restored as 1.5× and not as the 0 the
                // transport reports while paused (P2-5).
                if self.player.timeControlStatus == .playing && self.player.rate > 0 {
                    self.preferredRate = self.player.rate
                    if self.realPositionMs() >= self.attachedAtPositionMs + 5_000 {
                        self.establishedPlayback = true
                        self.sameDeliveryStallRecovery.reset()
                        // The most recent same-delivery recovery proved itself.
                        // A later, independent interruption may reconnect once
                        // too; an immediate repeated failure remains terminal.
                        self.establishedHDRRetryAttempted = false
                    }
                }
                self.updateNowPlaying()
                if self.isPlaying && self.currentMs - self.lastReportedMs >= 10_000 {
                    self.lastReportedMs = self.currentMs
                    self.report(self.currentMs)
                }
            }
        }
    }

    private func observeEnd(of item: AVPlayerItem) {
        if let endObserver { NotificationCenter.default.removeObserver(endObserver) }
        endObserver = NotificationCenter.default.addObserver(
            forName: .AVPlayerItemDidPlayToEndTime,
            object: item,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in
                guard let self,
                      self.started,
                      !self.isChangingStream,
                      self.player.currentItem === item
                else { return }
                let endedAt = self.realPositionMs()
                // A growing EVENT playlist can momentarily end before the
                // title does. Only hand autoplay a genuine film/episode end.
                if self.knownDurationMs > 0 && endedAt < self.knownDurationMs - 15_000 {
                    // The viewer did not pause — the playlist merely announced
                    // its current end — and `wantsPlayback` still says so, so
                    // this continuation keeps playing.
                    await self.reopen(at: endedAt)
                    return
                }
                self.isPlaying = false
                self.wantsPlayback = false
                self.currentMs = self.knownDurationMs > 0 ? self.knownDurationMs : endedAt
                self.report(self.currentMs)
                self.updateNowPlaying()
                self.finished = true
            }
        }
    }

    private func observeStatus(of item: AVPlayerItem) {
        itemStatusObservation = item.observe(\.status, options: [.initial, .new]) { [weak self] item, _ in
            guard item.status == .failed else { return }
            Task { @MainActor in
                guard let self, self.player.currentItem === item else { return }
                self.reportPlaybackFailure(item)
                if self.started {
                    // P2-6: this item is already dead, so its `currentTime()`
                    // is 0 or invalid and a VOD/direct retry would silently
                    // restart the film at 0:00. The last position the periodic
                    // observer saw is the truthful retry point. The transport
                    // intent survives in `wantsPlayback`, so a viewer who was
                    // paused when the item failed stays paused.
                    let position = Self.compatibilityRetryPositionMs(lastObservedMs: self.currentMs)
                    if await self.retryEstablishedHDRDelivery(at: position) { return }
                    let event = item.errorLog()?.events.last
                    if Self.isCompatibilityPlaybackFailure(
                        error: item.error as NSError?,
                        eventDomain: event?.errorDomain,
                        eventStatus: event?.errorStatusCode,
                        eventComment: event?.errorComment
                    ), await self.retryWithNextCompatibilityFallback(at: position) { return }
                }
                self.player.pause()
                self.isPlaying = false
                self.wantsPlayback = false
                self.isChangingStream = false
                self.failed = true
                self.playbackError = item.error?.localizedDescription
                    ?? PlaybackPreparationError.failed.localizedDescription
            }
        }
    }

    private func reportPlaybackFailure(_ item: AVPlayerItem) {
        #if os(iOS)
        if offlineId != nil { return }
        #endif
        let failure = item.error as NSError?
        let event = item.errorLog()?.events.last
        let payload = ApplePlaybackFailureLog(
            message: failure?.localizedDescription
                ?? PlaybackPreparationError.failed.localizedDescription,
            method: clientLogMethod,
            code: failure?.code,
            title: title,
            fileId: fileId,
            vcodec: decision?.source?.videoCodec,
            detail: Self.playbackFailureDetail(
                error: failure,
                eventDomain: event?.errorDomain,
                eventStatus: event?.errorStatusCode,
                eventComment: event?.errorComment
            )
        )
        postClientLog(payload)
    }

    private func reportPlaybackStall(
        _ event: PlaybackStallEvent,
        outcome: SameDeliveryStallRecoveryOutcome
    ) {
        #if os(iOS)
        if offlineId != nil { return }
        #endif
        let payload = ApplePlaybackStallLog(
            kind: event.kind,
            outcome: outcome,
            positionMs: event.positionMs,
            durationMs: event.durationMs,
            method: clientLogMethod,
            title: title,
            fileId: fileId,
            vcodec: decision?.source?.videoCodec,
            encoder: encoder
        )
        postClientLog(payload)
    }

    private func postClientLog<Payload: Encodable>(_ payload: Payload) {
        guard let url = Session.shared.url("/api/v1/client-log"),
              let body = try? JSONEncoder().encode(payload)
        else { return }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.httpBody = body
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        Session.shared.authorize(&request)
        Task {
            _ = try? await URLSession.shared.data(for: request)
        }
    }

    static func playbackFailureDetail(
        error: NSError?,
        eventDomain: String?,
        eventStatus: Int?,
        eventComment: String?
    ) -> String {
        var fields: [String] = []
        if let error {
            fields.append("error=\(error.domain):\(error.code)")
            if let underlying = error.userInfo[NSUnderlyingErrorKey] as? NSError {
                fields.append(
                    "underlying=\(underlying.domain):\(underlying.code) \(underlying.localizedDescription)"
                )
            }
        }
        if let eventDomain, let eventStatus {
            fields.append("event=\(eventDomain):\(eventStatus)")
        } else if let eventDomain {
            fields.append("event=\(eventDomain)")
        } else if let eventStatus {
            fields.append("event_status=\(eventStatus)")
        }
        if let eventComment, !eventComment.isEmpty {
            fields.append("comment=\(eventComment)")
        }
        return fields.joined(separator: " · ")
    }

    static func shouldRetryWithCompatibilityTranscode(
        canRetry: Bool,
        alreadyAttempted: Bool
    ) -> Bool {
        canRetry && !alreadyAttempted
    }

    /// AVPlayer owns ordinary buffering and resumes it when enough media has
    /// arrived. A stopped film clock while the player explicitly reports
    /// `.waitingToPlayAtSpecifiedRate` therefore cannot be used as decoder
    /// evidence. This applies equally to iPhone, iPad, and Apple TV.
    static func shouldMonitorSilentPlaybackStall(
        timeControlStatus: AVPlayer.TimeControlStatus
    ) -> Bool {
        timeControlStatus != .waitingToPlayAtSpecifiedRate
    }

    /// A network wait gets its own bounded recovery timer. It is intentionally
    /// separate from `shouldMonitorSilentPlaybackStall`: the caller routes its
    /// recovery straight back to the same delivery and never treats it as
    /// evidence for a codec/HDR fallback.
    static func shouldMonitorBufferingStall(
        timeControlStatus: AVPlayer.TimeControlStatus
    ) -> Bool {
        timeControlStatus == .waitingToPlayAtSpecifiedRate
    }

    /// Only a media/container/decoder rejection may advance the compatibility
    /// ladder. Timeouts, HTTP failures, and other transport errors must never
    /// spend the HDR fallbacks merely because they happened before first frame.
    static func isCompatibilityPlaybackFailure(
        error: NSError?,
        eventDomain: String?,
        eventStatus: Int?,
        eventComment: String?
    ) -> Bool {
        var chain: [NSError] = []
        var next = error
        while let current = next, chain.count < 8 {
            chain.append(current)
            next = current.userInfo[NSUnderlyingErrorKey] as? NSError
        }

        let avMediaCodes: Set<AVError.Code> = [
            .decodeFailed,
            .invalidSourceMedia,
            .fileFormatNotRecognized,
            .fileFailedToParse,
            .decoderNotFound,
            .incompatibleAsset,
            .failedToParse,
            .undecodableMediaData,
            .formatUnsupported,
        ]
        if chain.contains(where: { candidate in
            guard candidate.domain == AVFoundationErrorDomain,
                  let code = AVError.Code(rawValue: candidate.code)
            else { return false }
            return avMediaCodes.contains(code)
        }) {
            return true
        }

        // VideoToolbox decoder failures commonly surface as an underlying
        // CoreMedia/OSStatus error instead of an AVError.Code.
        let decoderStatusCodes: Set<Int> = [
            -12906, // kVTCouldNotFindVideoDecoderErr
            -12909, // kVTVideoDecoderBadDataErr
            -12910, // kVTVideoDecoderUnsupportedDataFormatErr
            -12911, // kVTVideoDecoderMalfunctionErr
            -17694, // kVTVideoDecoderReferenceMissingErr
        ]
        if chain.contains(where: { decoderStatusCodes.contains($0.code) })
            || eventStatus.map(decoderStatusCodes.contains) == true {
            return true
        }

        let diagnostic = ([eventDomain, eventComment].compactMap { $0 }
            + chain.flatMap { [$0.domain, $0.localizedDescription] })
            .joined(separator: " ")
            .lowercased()
        let mediaTerms = [
            "decoder failed",
            "decoder rejected",
            "could not decode",
            "codec is not supported",
            "codec not supported",
            "format is not supported",
            "format not supported",
            "undecodable media",
        ]
        return mediaTerms.contains(where: diagnostic.contains)
    }

    static func nextCompatibilityFallback(
        canRetryWithHDRBase: Bool,
        hdrBaseAlreadyAttempted: Bool,
        canRetryWithTranscode: Bool,
        transcodeAlreadyAttempted: Bool
    ) -> PlaybackCompatibilityFallback {
        if canRetryWithHDRBase && !hdrBaseAlreadyAttempted { return .hdrBase }
        if shouldRetryWithCompatibilityTranscode(
            canRetry: canRetryWithTranscode,
            alreadyAttempted: transcodeAlreadyAttempted
        ) {
            return .transcode
        }
        return .none
    }

    /// Profile 8.1/8.4 sources carry a complete HDR base picture beneath
    /// their Dolby Vision metadata. If VideoToolbox silently rejects that
    /// title's RPU/configuration, the server can strip only the DV layer and
    /// keep the same 10-bit HEVC picture instead of tone-mapping to SDR.
    static func hasCompatibleDolbyVisionBase(_ hdrFormat: String?) -> Bool {
        let format = hdrFormat?.lowercased() ?? ""
        return format.contains("hdr10-compatible") || format.contains("hlg-compatible")
    }

    static let hdrSubtitleNotice =
        "That subtitle requires an SDR burn-in. HDR playback was kept unchanged."
    static let pgsOverlayExternalPlaybackNotice =
        "PGS overlays stay in the app and are not available in Picture in Picture, AirPlay, or external playback. HDR playback was kept unchanged."

    static func isHDRDelivery(_ deliveredRange: String?) -> Bool {
        guard let range = deliveredRange?.lowercased() else { return false }
        return ["dolby_vision", "hdr10", "hlg"].contains(range)
    }

    /// Burn-only bitmap and styled subtitles can only be drawn by the server's
    /// H.264 SDR pipeline. A recognized PGS application overlay is not a burn;
    /// refuse only the selections that would actually replace HDR video.
    static func subtitleBurnWouldDiscardHDR(
        _ index: Int?,
        tracks: [SubtitleTrack],
        deliveredRange: String?
    ) -> Bool {
        guard let index, isHDRDelivery(deliveredRange) else { return false }
        return subtitleRequiresBurn(index, in: tracks)
    }

    static func shouldPreserveEstablishedHDRDelivery(
        deliveredRange: String?,
        establishedPlayback: Bool
    ) -> Bool {
        establishedPlayback && isHDRDelivery(deliveredRange)
    }

    /// A stream that has already rendered real HDR did not fail capability
    /// negotiation. Reconnect the same recipe once; if it immediately fails
    /// again, stop visibly instead of hiding the transport fault behind SDR.
    private func retryEstablishedHDRDelivery(at position: Int) async -> Bool {
        guard Self.shouldPreserveEstablishedHDRDelivery(
            deliveredRange: deliveredRange,
            establishedPlayback: establishedPlayback
        ) else { return false }
        guard !establishedHDRRetryAttempted else {
            player.pause()
            isPlaying = false
            wantsPlayback = false
            isChangingStream = false
            failed = true
            playbackError =
                "The HDR stream stopped responding. Playback was stopped instead of switching to SDR."
            return true
        }
        establishedHDRRetryAttempted = true
        isChangingStream = false
        await reopen(at: position)
        return true
    }

    /// Advance through the least-destructive recovery ladder. A compatible
    /// DV source first becomes HDR10/HLG without a video encode; only a second
    /// failure falls back to the existing universal transcode.
    private func retryWithNextCompatibilityFallback(at position: Int) async -> Bool {
        switch Self.nextCompatibilityFallback(
            canRetryWithHDRBase: canRetryCurrentItemWithHDRBase,
            hdrBaseAlreadyAttempted: dolbyVisionFallbackAttempted,
            canRetryWithTranscode: canRetryCurrentItemWithTranscode,
            transcodeAlreadyAttempted: compatibilityFallbackAttempted
        ) {
        case .none:
            return false
        case .hdrBase:
            dolbyVisionFallbackAttempted = true
            forceCompatibleHDRBase = true
            canRetryCurrentItemWithHDRBase = false
            isChangingStream = false
            playbackError = "Dolby Vision did not start. Retrying the HDR10-compatible picture…"
        case .transcode:
            compatibilityFallbackAttempted = true
            forceCompatibilityTranscode = true
            canRetryCurrentItemWithTranscode = false
            isChangingStream = false
            playbackError = "The compatible stream did not start. Retrying a universal stream…"
        }
        await reopen(at: position)
        return true
    }

    private func report(_ position: Int) {
        guard position > 0 else { return }
        let duration = knownDurationMs > 0 ? knownDurationMs : nil
        #if os(iOS)
        if let offlineId {
            Task {
                await OfflineDownloadManager.shared.recordProgress(
                    id: offlineId,
                    positionMs: position,
                    durationMs: duration
                )
            }
            return
        }
        #endif
        let itemId = itemId
        let model = model
        Task { await model?.reportProgress(itemId: itemId, positionMs: position, durationMs: duration) }
    }

    private func seekWhenReady(_ item: AVPlayerItem, ms: Int) async throws {
        // AVPlayerItem's KVO publisher is not guaranteed to deliver another
        // value when a tvOS network request stalls. The old unbounded
        // `for await` consequently held the initial `play()` forever and left
        // the transport looking paused. Poll the authoritative status with a
        // finite deadline so playback either resumes or surfaces a useful
        // connection error.
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: .seconds(15))
        while item.status == .unknown {
            try Task.checkCancellation()
            guard clock.now < deadline else { throw PlaybackPreparationError.timedOut }
            try await Task.sleep(for: .milliseconds(100))
        }

        if item.status == .failed {
            throw item.error ?? PlaybackPreparationError.failed
        }
        guard item.status == .readyToPlay else { throw PlaybackPreparationError.failed }

        _ = await player.seek(
            to: CMTime(seconds: Double(ms) / 1000.0, preferredTimescale: 600),
            toleranceBefore: .zero,
            toleranceAfter: .zero
        )
    }

    /// Position in the HLS master rendition order. The server advertises only
    /// native tracks and preserves source order, so this remains stable even
    /// when bitmap and styled tracks are interleaved in the menu.
    static func nativeSubtitleOrdinal(_ index: Int, in tracks: [SubtitleTrack]) -> Int? {
        tracks.filter(\.isNativeHLS).firstIndex(where: { $0.index == index })
    }

    static func subtitleRequiresBurn(_ index: Int, in tracks: [SubtitleTrack]) -> Bool {
        tracks.first(where: { $0.index == index }).map {
            !$0.isNativeHLS && !$0.isPGSOverlay
        } ?? true
    }

    static func subtitleUsesOverlay(_ index: Int?, in tracks: [SubtitleTrack]) -> Bool {
        guard let index else { return false }
        return tracks.first(where: { $0.index == index })?.isPGSOverlay == true
    }

    /// Pure media-selection step used by the AVPlayer adapter and XCTest. A
    /// successful result means the video item stays in place; the closure gets
    /// nil for Off or the native rendition ordinal for a selected track.
    @discardableResult
    static func applyNativeSubtitleSelection(
        _ index: Int?,
        tracks: [SubtitleTrack],
        select: (Int?) -> Void
    ) -> Bool {
        guard let index else {
            select(nil)
            return true
        }
        guard let ordinal = nativeSubtitleOrdinal(index, in: tracks) else { return false }
        select(ordinal)
        return true
    }

    /// Which position in the legible group's subtitle options carries native
    /// rendition `ordinal`. Positional mapping is a bet with no contractual
    /// basis: AVFoundation may merge same-`NAME` entries and may synthesize a
    /// closed-caption option, and either event shifts every ordinal. Match the
    /// `LANGUAGE`/`NAME` pair the server actually authored first, and keep the
    /// ordinal only as the last resort (P1-1).
    static func nativeSubtitleOptionIndex(
        ordinal: Int,
        tracks: [SubtitleTrack],
        options: [SubtitleRenditionOption]
    ) -> Int? {
        let natives = tracks.filter(\.isNativeHLS)
        guard natives.indices.contains(ordinal) else { return nil }
        let track = natives[ordinal]
        let names = subtitleRenditionNames(tracks)
        guard names.indices.contains(ordinal) else { return nil }
        let name = quotedAttributeValue(names[ordinal])
        let tag = languageCode(subtitleLanguageTag(track.language))
        if let match = options.firstIndex(where: {
            $0.displayName == name && languageCode($0.languageTag) == tag
        }) {
            return match
        }
        if let match = options.firstIndex(where: { $0.displayName == name }) { return match }
        let sameLanguage = options.indices.filter { languageCode(options[$0].languageTag) == tag }
        if sameLanguage.count == 1 { return sameLanguage[0] }
        return options.indices.contains(ordinal) ? ordinal : nil
    }

    /// Every rendition `NAME` the master advertises, in master order — the
    /// replica of `unique_subtitle_names` in crates/plurxd/src/http/hls.rs.
    ///
    /// RFC 8216 makes NAME unique within a group, so the server disambiguates
    /// repeats by occurrence ("English", "English (2)"). Replicating only the
    /// base name would make two untitled same-language tracks collide and the
    /// second one resolve onto the first — worse than the ordinal guess this
    /// matching replaced.
    static func subtitleRenditionNames(_ tracks: [SubtitleTrack]) -> [String] {
        var seen: [String: Int] = [:]
        var names: [String] = []
        for (position, track) in tracks.enumerated() where track.isNativeHLS {
            let base = subtitleRenditionName(track, position: position)
            let count = (seen[base] ?? 0) + 1
            seen[base] = count
            names.append(count == 1 ? base : "\(base) (\(count))")
        }
        return names
    }

    /// Replica of the server's `quoted`: `NAME` is an HLS quoted-string, so
    /// characters that cannot appear in one are rewritten before emission and
    /// a title containing them reaches AVFoundation already rewritten.
    static func quotedAttributeValue(_ value: String) -> String {
        String(value.map { character -> Character in
            switch character {
            case "\"": return "'"
            case "\r", "\n": return " "
            default: return character
            }
        })
    }

    /// Replica of the server's `subtitle_name` (crates/plurxd/src/http/hls.rs)
    /// — the base `NAME` for the track at `position` in the decision's
    /// subtitle list, before de-duplication. The separator is U+00B7 MIDDLE
    /// DOT with one space on either side, exactly as the server emits it.
    static func subtitleRenditionName(_ track: SubtitleTrack, position: Int) -> String {
        let language = subtitleLanguageName(track.language)
        let title = track.title?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let title, !title.isEmpty {
            return language == "und" ? title : "\(language) · \(title)"
        }
        return language == "und" ? "Subtitle \(position + 1)" : language
    }

    /// Replica of the server's `language_tag`, which is one line delegating to
    /// `plurx_core::tracks::bcp47_tag`: absent or blank is "und", any spelling
    /// the alias table knows becomes its two-letter member (taken by length,
    /// because the groups are ordered by settings canonicality), and an
    /// unknown code passes through unchanged.
    static func subtitleLanguageTag(_ raw: String?) -> String {
        let trimmed = raw?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard !trimmed.isEmpty else { return "und" }
        let lower = trimmed.lowercased()
        return languageAliases.first(where: { $0.contains(lower) })?
            .first(where: { $0.count == 2 }) ?? trimmed
    }

    /// Replica of the server's `language_name`: the display half of `NAME`,
    /// kept in step with the alias table so a language that matches never
    /// renders as a bare three-letter code.
    static func subtitleLanguageName(_ raw: String?) -> String {
        switch subtitleLanguageTag(raw) {
        case "en": return "English"
        case "it": return "Italian"
        case "ja": return "Japanese"
        case "es": return "Spanish"
        case "fr": return "French"
        case "de": return "German"
        case "pt": return "Portuguese"
        case "ko": return "Korean"
        case "zh": return "Chinese"
        case "ru": return "Russian"
        case "hi": return "Hindi"
        case "ar": return "Arabic"
        case "nl": return "Dutch"
        case "sv": return "Swedish"
        case "pl": return "Polish"
        case "no": return "Norwegian"
        case "da": return "Danish"
        case "fi": return "Finnish"
        case "tr": return "Turkish"
        case "th": return "Thai"
        case "vi": return "Vietnamese"
        case "uk": return "Ukrainian"
        case "cs": return "Czech"
        case "el": return "Greek"
        case "he": return "Hebrew"
        case "hu": return "Hungarian"
        case "ro": return "Romanian"
        case let other: return other
        }
    }

    /// Guardrail (plan §6.4): sending `subtitle_burn` for a track a current
    /// server classifies native recreates the exact bug this arc removed, so
    /// the P1-3 fallback needs positive evidence that the server predates
    /// `native_subtitles` — a create response with no native master query
    /// *and* an asset advertising no subtitle rendition at all. A selection
    /// that merely failed is not evidence, and direct play never qualifies:
    /// there is no create response to have judged.
    static func serverIsLegacy(
        servesNative: Bool,
        hasSubtitleOptions: Bool,
        isDirect: Bool
    ) -> Bool {
        !servesNative && !hasSubtitleOptions && !isDirect
    }

    /// A server that predates native subtitles hands back the plain session
    /// playlist; a current one carries the native master query. This is the
    /// evidence the P1-3 legacy fallback is gated on.
    static func playlistAdvertisesNativeSubtitles(_ playlistUrl: String) -> Bool {
        guard let items = URLComponents(string: playlistUrl)?.queryItems else { return false }
        return items.contains { $0.name == "native" && $0.value != "0" }
    }

    /// Resolve player-local time zero onto the source timeline.
    ///
    /// Copy sessions can start at the keyframe before the requested seek, so
    /// the server's integer origin wins. The older floating-point request
    /// echo remains the compatibility fallback. Cached whole-title sessions
    /// use direct timeline semantics and always begin at zero.
    nonisolated static func sessionMediaOriginMs(_ hls: HlsStart, requestedStartMs: Int) -> Int {
        if hls.vod == true { return 0 }
        if let mediaOriginMs = hls.mediaOriginMs { return mediaOriginMs }
        return Int((hls.startSeconds ?? Double(requestedStartMs) / 1000.0) * 1000)
    }

    private func applyNativeSubtitleSelection(_ index: Int?, to item: AVPlayerItem?) async {
        guard let item, player.currentItem === item else { return }
        let group = try? await item.asset.loadMediaSelectionGroup(for: .legible)
        guard started, player.currentItem === item else { return }
        if let group, selectNativeSubtitle(index, in: group, of: item) { return }
        // Off never fails: without a legible group nothing is being rendered.
        guard let index else { return }
        // "Has a legible group" is not "advertises subtitle renditions".
        // AVFoundation may synthesise a closed-caption option into `.legible`
        // for a variant that carries no CLOSED-CAPTIONS attribute, so a legacy
        // master can hand back a non-empty group with no subtitles in it.
        let hasSubtitleOptions = group?.options.contains { $0.mediaType == .subtitle } ?? false
        await recoverFromFailedNativeSelection(
            index,
            item: item,
            hasSubtitleOptions: hasSubtitleOptions
        )
    }

    /// False when the selection could not be resolved onto a real option.
    private func selectNativeSubtitle(
        _ index: Int?,
        in group: AVMediaSelectionGroup,
        of item: AVPlayerItem
    ) -> Bool {
        // Only subtitle options are ours: AVFoundation can also expose a
        // closed-caption option that no rendition in the master authored.
        let options = group.options.filter { $0.mediaType == .subtitle }
        let descriptors = options.map {
            SubtitleRenditionOption(
                languageTag: $0.extendedLanguageTag,
                displayName: $0.displayName
            )
        }
        var resolved = false
        let eligible = Self.applyNativeSubtitleSelection(index, tracks: subtitles) { ordinal in
            guard let ordinal else {
                item.select(nil, in: group)
                resolved = true
                return
            }
            guard let position = Self.nativeSubtitleOptionIndex(
                ordinal: ordinal,
                tracks: subtitles,
                options: descriptors
            ), options.indices.contains(position) else { return }
            item.select(options[position], in: group)
            resolved = true
        }
        return eligible && resolved
    }

    /// P1-3: the selection did not land. Never leave state claiming a track
    /// the player is not rendering — either burn it (legacy servers only) or
    /// say so and drop the checkmark.
    private func recoverFromFailedNativeSelection(
        _ index: Int,
        item: AVPlayerItem,
        hasSubtitleOptions: Bool
    ) async {
        // The item itself failing is the status observer's story, not ours.
        guard item.status != .failed, selectedSubtitle == index else { return }
        let serverIsLegacy = Self.serverIsLegacy(
            servesNative: serverServesNativeSubtitles,
            hasSubtitleOptions: hasSubtitleOptions,
            isDirect: isDirectPlayback
        )
        let isText = subtitles.first(where: { $0.index == index })?.text ?? false
        if serverIsLegacy && isText {
            forceLegacySubtitleBurn = true
            let position = realPositionMs()
            if isChangingStream {
                // Called from inside `open()`, which `reopen()` refuses to
                // overlap. Run the burn once this open has finished.
                Task { [weak self] in await self?.reopen(at: position) }
            } else {
                await reopen(at: position)
            }
            return
        }
        selectedSubtitle = nil
        showPlaybackNotice("That subtitle track could not be turned on.")
    }

    /// The audible half of owning media selection (P2-8). An HLS session
    /// carries one muxed audio track — the server already chose it, and
    /// `selectAudio` reopens to change it — but a direct-play file can carry
    /// several, and with automatic criteria off nothing else would honor the
    /// viewer's language there. Best effort: no match leaves AVPlayer's own
    /// default in place, which is what mismatched criteria used to produce.
    private func applyPreferredAudioSelection(to item: AVPlayerItem) async {
        guard audioOverride == nil else { return }
        guard let group = try? await item.asset.loadMediaSelectionGroup(for: .audible),
              started, player.currentItem === item else { return }
        let preferred = AVMediaSelectionGroup.mediaSelectionOptions(
            from: group.options,
            filteredAndSortedAccordingToPreferredLanguages: bcp47(audioLanguage)
        )
        guard let option = preferred.first else { return }
        item.select(option, in: group)
    }

    #if os(iOS)
    private func titleMetadata(_ title: String) -> AVMetadataItem {
        let metadata = AVMutableMetadataItem()
        metadata.identifier = .commonIdentifierTitle
        metadata.value = title as NSString
        metadata.extendedLanguageTag = "und"
        return metadata
    }
    #endif

    private func bcp47(_ code: String) -> [String] {
        Self.languageSpellings(code)
    }

    /// The audio index an HLS session must carry. `selected` is the server's
    /// automatic answer from `/decision`; `explicit` is a later viewer choice.
    nonisolated static func sessionAudioIndex(explicit: Int?, selected: Int?) -> Int? {
        explicit ?? selected
    }

    /// Every spelling of a language preference AVFoundation might have to
    /// match, BCP-47 tag first. Derived from the shared alias table rather
    /// than a third private copy of it — the twelve-language copy this
    /// replaced could not match a Dutch, Czech, Greek, or Romanian asset.
    static func languageSpellings(_ code: String) -> [String] {
        let lower = code.lowercased()
        guard let group = languageAliases.first(where: { $0.contains(lower) }) else {
            return [code]
        }
        let tag = group.first(where: { $0.count == 2 }) ?? lower
        return [tag] + group.filter { $0 != tag }
    }

    /// The subtitle the player may enable automatically: **the one the server
    /// already chose**, subject to one veto.
    ///
    /// Choosing is the server's job (decided by Paul on 2026-08-03).
    /// `/decision` runs the shared `select_tracks` — anime dual-audio, the
    /// language preferences, and the subtitle mode Auto/Always/Off — and then
    /// stamps `default: true` on exactly the track it picked
    /// (crates/plurxd/src/http/stream.rs, decision handler). `default` on a
    /// decision track is therefore the server's answer and not the muxer's
    /// flag. This client used to re-derive the same rule from its own
    /// language settings, which is how it came to behave as `Always` against a
    /// server set to `Auto`: the rule lived in three languages and drifted.
    ///
    /// What stays here is a veto, not a policy — a statement about what this
    /// client can do rather than about which subtitle the viewer wants:
    /// **automatic selection must never start a burn, except for a forced
    /// track, which may, always at source height** (owner policy, plan §3.3).
    /// A server that picks a non-forced PGS/VobSub/ASS track gets nothing
    /// selected rather than a video encoder on every play (P0-1); the viewer
    /// can still choose it by hand, which is the viewer asking.
    static func automaticSubtitleIndex(_ tracks: [SubtitleTrack]) -> Int? {
        guard let picked = tracks.first(where: { $0.default }) else { return nil }
        guard picked.isNativeHLS || isForcedSubtitle(picked) else { return nil }
        return picked.index
    }

    /// Both signals are meaningful: file 5615's English forced track carries
    /// `forced=false` and only the title says so.
    static func isForcedSubtitle(_ track: SubtitleTrack) -> Bool {
        track.forced || (track.title.map { titleMarksForced($0) } ?? false)
    }

    /// Replica of the server's `title_marks_forced` (crates/plurxd/src/http/
    /// hls.rs). A substring test is too eager: "Non-Forced" and "Unforced" are
    /// real titles, and here the forced arm is the *only* path by which
    /// automatic selection may start a burn, so an over-eager test burns an
    /// ordinary PGS track on every play. Match "forced" on word boundaries and
    /// reject it when the preceding word negates it; "Unforced" and
    /// "Reinforced" fall out for free because the letter in front of them is
    /// not a boundary. Plain "Forced" keeps working — that is 5615's contract.
    static func titleMarksForced(_ title: String) -> Bool {
        let lower = title.lowercased()
        var searchStart = lower.startIndex
        while let found = lower.range(of: "forced", range: searchStart..<lower.endIndex) {
            let boundedBefore = found.lowerBound == lower.startIndex
                || !isAlphanumeric(lower[lower.index(before: found.lowerBound)])
            let boundedAfter = found.upperBound == lower.endIndex
                || !isAlphanumeric(lower[found.upperBound])
            if boundedBefore, boundedAfter,
               !negatedBefore(String(lower[lower.startIndex..<found.lowerBound])) {
                return true
            }
            searchStart = found.upperBound
        }
        return false
    }

    /// Replica of the server's `negated_before`: the word immediately in front
    /// of a "forced" occurrence, when it turns the claim around. Separators are
    /// skipped, so "non-forced", "non forced", and "not forced" share a rule.
    private static func negatedBefore(_ prefix: String) -> Bool {
        let word = prefix
            .split(whereSeparator: { !isAlphanumeric($0) })
            .last
            .map(String.init) ?? ""
        return ["non", "not", "no", "never"].contains(word)
    }

    private static func isAlphanumeric(_ character: Character) -> Bool {
        character.isLetter || character.isNumber
    }

    /// A subtitle burn on a file the device could otherwise take keeps source
    /// resolution — that is the carve-out which lets an automatically selected
    /// forced bitmap track burn without also downgrading the picture. A
    /// genuine transcode still lets server Auto choose its rung unless the
    /// viewer selected one explicitly.
    static func burnSessionHeight(
        burnSubtitle: Int?,
        mode: String,
        selectedHeight: Int?,
        sourceHeight: Int?
    ) -> Int? {
        if let selectedHeight { return selectedHeight }
        guard burnSubtitle != nil, mode != "transcode" else { return nil }
        return sourceHeight
    }

    /// The `subtitle_burn` / `subtitle` pair one selection puts on the wire.
    /// A native text track never becomes a burn — including on the
    /// compatibility-transcode retry, which keeps the selection it had — and
    /// the two fields are never sent together.
    static func sessionSubtitleFields(
        selected: Int?,
        tracks: [SubtitleTrack],
        legacyBurn: Bool
    ) -> (burn: Int?, native: Int?) {
        guard let selected else { return (nil, nil) }
        if subtitleUsesOverlay(selected, in: tracks) { return (nil, nil) }
        // A legacy server (P1-3) advertises no native renditions at all, so
        // its text tracks return to the pre-branch burn path.
        if legacyBurn || subtitleRequiresBurn(selected, in: tracks) {
            return (selected, nil)
        }
        return (nil, nativeSubtitleOrdinal(selected, in: tracks) == nil ? nil : selected)
    }

    /// Whether a reopen comes back playing. The first attach has no item yet
    /// and always starts; otherwise the viewer's own intent decides, which is
    /// deliberately not the player's observed state — a buffering player
    /// reports `.waitingToPlay` and a failed item has already dropped its rate
    /// to 0, so both would read as "paused" during exactly the reopens P2-5
    /// and P2-6 are about.
    static func reopenResumesPlayback(wantsPlayback: Bool, hasCurrentItem: Bool) -> Bool {
        !hasCurrentItem || wantsPlayback
    }

    /// The film position a compatibility retry resumes from. The failed item's
    /// own clock reads 0 or invalid, so the last position the periodic
    /// observer saw is the only truthful one (P2-6).
    static func compatibilityRetryPositionMs(lastObservedMs: Int) -> Int {
        max(lastObservedMs, 0)
    }

    /// Whether this open has to go through an HLS session for no reason other
    /// than making the file's text subtitles selectable. The whole of the
    /// `SubtitleReadiness` setting is this function; nothing else branches on
    /// it.
    ///
    /// `.onDemand` is the default (Paul, 2026-08-02, plan §2.5): a file with
    /// native text tracks direct-plays until one is actually asked for, so a
    /// play that never touches subtitles costs the server nothing and keeps
    /// true direct play — and the first selection pays for exactly one clean
    /// restart, the same one a burn already performs, at the same film
    /// position. `.instant` answers yes for any file carrying a native text
    /// track, which is the v0.2 behaviour: every track exists as a rendition
    /// before the menu is ever opened, so switching one is free.
    ///
    /// `subtitlesInUse` is sticky for the title, so turning subtitles back off
    /// does not drop the stream to direct play and charge the next selection a
    /// second restart.
    ///
    /// A file with no native text track answers no under either setting: there
    /// is nothing a session could publish. Bitmap and styled tracks are not
    /// native, so they cannot drag a direct-playable file into a session it
    /// gains nothing from.
    static func needsNativeSubtitleSession(
        hasNativeTextTrack: Bool,
        readiness: SubtitleReadiness,
        subtitlesInUse: Bool
    ) -> Bool {
        guard hasNativeTextTrack else { return false }
        switch readiness {
        case .instant: return true
        case .onDemand: return subtitlesInUse
        }
    }

    /// What one subtitle selection costs. A burn — or leaving one — replaces
    /// the video frames, and P2-7's direct-play → session boundary needs the
    /// session to exist before a native rendition can be selected at all.
    static func subtitleSelectionRoute(
        for index: Int?,
        tracks: [SubtitleTrack],
        activeBurn: Int?,
        isDirectPlayback: Bool,
        activeOverlay: Int? = nil
    ) -> SubtitleSelectionRoute {
        let needsBurn = index.map { subtitleRequiresBurn($0, in: tracks) } ?? false
        let targetUsesOverlay = subtitleUsesOverlay(index, in: tracks)
        let leavesDirectPlay = isDirectPlayback && index != nil && !targetUsesOverlay
        if needsBurn || activeBurn != nil || leavesDirectPlay { return .reopen }
        if targetUsesOverlay || (activeOverlay != nil && index == nil) { return .bitmapOverlay }
        return .mediaSelection
    }

    /// P1-2: what an `open()` still owes the viewer when it completes. The
    /// selection it applied was captured before its awaits, so a track chosen
    /// during a cold extraction has to be applied afterwards; nil means the
    /// stream already matches the selection the UI is showing.
    static func subtitleReconciliation(
        applied: Int?,
        current: Int?,
        tracks: [SubtitleTrack],
        activeBurn: Int?,
        isDirectPlayback: Bool,
        activeOverlay: Int? = nil
    ) -> SubtitleSelectionRoute? {
        guard applied != current else { return nil }
        return subtitleSelectionRoute(
            for: current,
            tracks: tracks,
            activeBurn: activeBurn,
            isDirectPlayback: isDirectPlayback,
            activeOverlay: activeOverlay
        )
    }

    /// ISO 639-1 / 639-2/B / 639-2/T spellings that mean the same language,
    /// mirrored group for group from `LANG_ALIASES` in
    /// crates/plurx-core/src/tracks.rs. The smaller copy this replaced knew
    /// twelve languages, so "dut"/"cze"/"gre"/"rum" never matched a viewer's
    /// "nl"/"cs"/"el"/"ro" — the same divergence plan §2.5's P2-2 filed
    /// against the server's own duplicate table.
    private static let languageAliases: [[String]] = [
        ["eng", "en"],
        ["jpn", "ja", "jp"],
        ["spa", "es"],
        ["fre", "fra", "fr"],
        ["ger", "deu", "de"],
        ["ita", "it"],
        ["por", "pt"],
        ["rus", "ru"],
        ["kor", "ko"],
        ["chi", "zho", "zh"],
        ["hin", "hi"],
        ["ara", "ar"],
        ["nld", "dut", "nl"],
        ["swe", "sv"],
        ["pol", "pl"],
        ["nor", "nob", "no"],
        ["dan", "da"],
        ["fin", "fi"],
        ["tur", "tr"],
        ["tha", "th"],
        ["vie", "vi"],
        ["ukr", "uk"],
        ["ces", "cze", "cs"],
        ["ell", "gre", "el"],
        ["heb", "he"],
        ["hun", "hu"],
        ["ron", "rum", "ro"],
    ]

    /// Collapse the spellings used by settings, ffprobe, and AVFoundation's
    /// `extendedLanguageTag` into the same comparison key. Region subtags are
    /// dropped ("en-US" is English) before the alias lookup.
    private static func languageCode(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let code = raw
            .lowercased()
            .replacingOccurrences(of: "_", with: "-")
            .split(separator: "-")
            .first
            .map(String.init) ?? ""
        guard !code.isEmpty else { return nil }
        return languageAliases.first(where: { $0.contains(code) })?
            .first(where: { $0.count == 2 }) ?? code
    }

    private nonisolated static func legacyMode(_ method: String) -> String {
        switch method {
        case "direct_play": return "direct"
        case "remux": return "remux"
        default: return "transcode"
        }
    }

    /// AVPlayer can accept a supported Dolby Vision Profile 5/8 decoder
    /// configuration from a progressive MP4, advance its clock, and still
    /// render a black video plane. Normalize every server-approved direct DV
    /// stream through copy-video HLS. This also protects clients talking to a
    /// server that predates the `dvhls=1` capability and still answers direct.
    nonisolated static func playbackMode(_ decision: Decision) -> String {
        let mode = decision.delivery?.mode ?? legacyMode(decision.method)
        guard mode == "direct", decision.source?.hdr?.lowercased() == "dolby_vision" else {
            return mode
        }
        return "remux"
    }

    /// Prefer the server's explicit preservation verdict. A legacy direct-DV
    /// answer is also safe to preserve: the server only returns direct after
    /// matching the exact profiles this client advertises (5 and 8).
    nonisolated static func shouldPreserveDolbyVision(_ decision: Decision) -> Bool {
        if let preserve = decision.delivery?.preserveDolbyVision
            ?? decision.preserveDolbyVision {
            return preserve
        }
        let serverMode = decision.delivery?.mode ?? legacyMode(decision.method)
        return serverMode == "direct"
            && decision.source?.hdr?.lowercased() == "dolby_vision"
    }

    #if os(iOS)
    private func installRemoteCommands() {
        let commands = MPRemoteCommandCenter.shared()
        commands.playCommand.isEnabled = true
        commands.pauseCommand.isEnabled = true
        commands.togglePlayPauseCommand.isEnabled = true
        commands.skipBackwardCommand.isEnabled = true
        commands.skipForwardCommand.isEnabled = true
        commands.changePlaybackPositionCommand.isEnabled = true
        commands.skipBackwardCommand.preferredIntervals = [10]
        commands.skipForwardCommand.preferredIntervals = [10]

        remoteTargets.append((commands.playCommand, commands.playCommand.addTarget { [weak self] _ in
            Task { @MainActor in
                self?.player.play()
                self?.isPlaying = true
                self?.wantsPlayback = true
                self?.updateNowPlaying()
            }
            return .success
        }))
        remoteTargets.append((commands.pauseCommand, commands.pauseCommand.addTarget { [weak self] _ in
            Task { @MainActor in
                self?.player.pause()
                self?.isPlaying = false
                self?.wantsPlayback = false
                self?.updateNowPlaying()
            }
            return .success
        }))
        remoteTargets.append((commands.togglePlayPauseCommand,
                              commands.togglePlayPauseCommand.addTarget { [weak self] _ in
            Task { @MainActor in self?.togglePlayPause() }
            return .success
        }))
        remoteTargets.append((commands.skipBackwardCommand,
                              commands.skipBackwardCommand.addTarget { [weak self] _ in
            Task { @MainActor in self?.skip(seconds: -10) }
            return .success
        }))
        remoteTargets.append((commands.skipForwardCommand,
                              commands.skipForwardCommand.addTarget { [weak self] _ in
            Task { @MainActor in self?.skip(seconds: 10) }
            return .success
        }))
        remoteTargets.append((commands.changePlaybackPositionCommand,
                              commands.changePlaybackPositionCommand.addTarget { [weak self] event in
            guard let event = event as? MPChangePlaybackPositionCommandEvent else {
                return .commandFailed
            }
            Task { @MainActor in self?.seek(toMs: Int(event.positionTime * 1000)) }
            return .success
        }))
    }

    private func removeRemoteCommands() {
        for (command, target) in remoteTargets { command.removeTarget(target) }
        remoteTargets = []
    }

    private func updateNowPlaying() {
        guard knownDurationMs > 0 else { return }
        let rate: Float = isPlaying ? max(player.rate, 1) : 0
        MPNowPlayingInfoCenter.default().nowPlayingInfo = [
            MPMediaItemPropertyTitle: title,
            MPMediaItemPropertyPlaybackDuration: Double(knownDurationMs) / 1000.0,
            MPNowPlayingInfoPropertyElapsedPlaybackTime: Double(realPositionMs()) / 1000.0,
            MPNowPlayingInfoPropertyPlaybackRate: rate,
            MPNowPlayingInfoPropertyDefaultPlaybackRate: 1.0,
            MPNowPlayingInfoPropertyIsLiveStream: false,
        ]
        MPNowPlayingInfoCenter.default().playbackState = isPlaying ? .playing : .paused
    }
    #else
    private func updateNowPlaying() {}
    #endif
}
