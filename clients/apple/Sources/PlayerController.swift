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

/// A repeated end notification at one early media boundary is a terminal
/// playback failure even though AVPlayer reports no NSError. Give it its own
/// event so server logs do not mislabel a playlist/timestamp failure as a
/// decoder failure.
struct ApplePlaybackEarlyEndLog: Encodable, Equatable {
    let level = "error"
    let event = "avplayer_early_end"
    let message: String
    let method: String
    let title: String
    let fileId: Int
    let vcodec: String?
    let detail: String
    let ua = "Apple AVPlayer"

    init(
        positionMs: Int,
        expectedDurationMs: Int?,
        isGrowingPlaylist: Bool,
        message: String,
        method: String,
        title: String,
        fileId: Int,
        vcodec: String?
    ) {
        self.message = message
        self.method = method
        self.title = title
        self.fileId = fileId
        self.vcodec = vcodec
        let expected = expectedDurationMs.map(String.init) ?? "unknown"
        detail = "position_ms=\(max(0, positionMs)) · expected_duration_ms=\(expected) · growing=\(isGrowingPlaylist) · outcome=terminal"
    }

    enum CodingKeys: String, CodingKey {
        case level, event, message, method, title, detail, ua
        case fileId = "file_id"
        case vcodec
    }
}

/// One start-or-resume time-to-first-frame measurement. The live HLS session
/// id is carried as a typed field so the server can join this client-observed
/// wait to the producer and pacing state that existed when the first frame
/// arrived.
struct ApplePlaybackTTFFLog: Encodable {
    let level = "info"
    let event = "ttff"
    let message: String
    let method: String
    let title: String
    let fileId: Int
    let vcodec: String?
    let ms: Int
    let height: Int?
    let encoder: String?
    let sessionId: String?
    let attempt: String
    let reason: String
    let ua = "Apple AVPlayer"

    init(
        ms: Int,
        method: String,
        title: String,
        fileId: Int,
        vcodec: String?,
        height: Int?,
        encoder: String?,
        sessionId: String?,
        attempt: String,
        reason: String
    ) {
        self.message = "first frame after \(max(0, ms)) ms"
        self.method = method
        self.title = title
        self.fileId = fileId
        self.vcodec = vcodec
        self.ms = max(0, ms)
        self.height = height
        self.encoder = encoder
        self.sessionId = sessionId
        self.attempt = attempt
        self.reason = reason
    }

    enum CodingKeys: String, CodingKey {
        case level, event, message, method, title, vcodec, ms, height, encoder
        case attempt, reason, ua
        case fileId = "file_id"
        case sessionId = "session_id"
    }
}

/// Monotonic, one-shot first-progress gate. AVPlayer readiness is not a frame,
/// and a playing intent can still be waiting for data, so the measurement ends
/// only when the film clock advances while AVPlayer reports actual playback.
struct ApplePlaybackTTFFState: Equatable {
    private var openedAt: TimeInterval?
    private var openedPositionMs = 0

    mutating func opened(
        at positionMs: Int,
        observedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) {
        openedPositionMs = max(0, positionMs)
        openedAt = observedAt
    }

    mutating func observe(
        positionMs: Int,
        playing: Bool,
        observedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) -> Int? {
        guard let openedAt,
              playing,
              positionMs >= openedPositionMs + 250
        else { return nil }
        self.openedAt = nil
        return max(0, Int(((observedAt - openedAt) * 1_000).rounded()))
    }

    /// Move only the film-position gate when the attached item starts at a
    /// keyframe before the request, or a viewer seeks before first progress.
    /// The monotonic start remains the original wait anchor.
    mutating func rebasePosition(at positionMs: Int) {
        guard openedAt != nil else { return }
        openedPositionMs = max(0, positionMs)
    }

    mutating func reset() {
        openedAt = nil
        openedPositionMs = 0
    }
}

/// Last server status the Apple client observed before a stall. Recovery can
/// supersede the session before the best-effort beacon reaches `/client-log`,
/// so the snapshot travels with the client evidence and the server replaces
/// it with a fresher live join only when that is still possible.
struct ApplePlaybackServerSnapshot: Encodable, Equatable {
    var observedAgeMs: Int?
    var recentSpeed: Double?
    var aheadSeconds: Int?
    var aheadBytes: Int?
    var suspended: Bool?
    var holdReason: String?
    var deliveredBps: Int?
    var deliveredIdleMs: Int?
    var readrate: Double?
    var suspendCount: Int?
    var progressIdleMs: Int?
    var publishedEndMs: Int?
    var fetchedEndMs: Int?
    var fetchedSegment: Int?
    var firstRetainedSegment: Int?
    var playlistShape: String?
    var lastRequest: String?
    var lastRequestIdleMs: Int?

    init(_ status: PlaybackSessionStatus, observedAgeMs: Int? = nil) {
        self.observedAgeMs = observedAgeMs
        recentSpeed = status.recentSpeed
        aheadSeconds = status.aheadSeconds
        aheadBytes = status.aheadBytes
        suspended = status.suspended
        holdReason = status.holdReason
        deliveredBps = status.deliveredBps
        deliveredIdleMs = status.deliveredIdleMs
        readrate = status.readrate
        suspendCount = status.suspendCount
        progressIdleMs = status.progressIdleMs
        publishedEndMs = status.publishedEndMs
        fetchedEndMs = status.fetchedEndMs
        fetchedSegment = status.fetchedSegment
        firstRetainedSegment = status.firstRetainedSegment
        playlistShape = status.playlistShape
        lastRequest = status.lastRequest
        lastRequestIdleMs = status.idleSeconds.map {
            let seconds = max(0, $0)
            return seconds > Int.max / 1_000 ? Int.max : seconds * 1_000
        }
    }

    enum CodingKeys: String, CodingKey {
        case observedAgeMs = "observed_age_ms"
        case recentSpeed = "recent_speed"
        case aheadSeconds = "ahead_seconds"
        case aheadBytes = "ahead_bytes"
        case suspended
        case holdReason = "hold_reason"
        case deliveredBps = "delivered_bps"
        case deliveredIdleMs = "delivered_idle_ms"
        case readrate
        case suspendCount = "suspend_count"
        case progressIdleMs = "progress_idle_ms"
        case publishedEndMs = "published_end_ms"
        case fetchedEndMs = "fetched_end_ms"
        case fetchedSegment = "fetched_segment"
        case firstRetainedSegment = "first_retained_segment"
        case playlistShape = "playlist_shape"
        case lastRequest = "last_request"
        case lastRequestIdleMs = "last_request_idle_ms"
    }
}

/// AVPlayer and access-log state sampled at the same point that declared the
/// clock stagnant. Runway separates an empty buffer from a decode/clock stop;
/// request and transfer counters separate a client that stopped fetching from
/// a server that had no segment to hand it.
struct ApplePlaybackDiagnosticSnapshot: Encodable, Equatable {
    var positionMs: Int?
    var runway: Double?
    var timeControlStatus: String?
    var waitingReason: String?
    var playbackBufferEmpty: Bool?
    var playbackLikelyToKeepUp: Bool?
    var playbackBufferFull: Bool?
    var mediaRequests: Int?
    var downloadedDuration: Double?
    var bytesTransferred: Int?
    var transferDuration: Double?
    var observedBitrateBps: Double?
    var indicatedBitrateBps: Double?
    var accessStalls: Int?
    var server: ApplePlaybackServerSnapshot?

    enum CodingKeys: String, CodingKey {
        case positionMs = "position_ms"
        case runway
        case timeControlStatus = "time_control_status"
        case waitingReason = "waiting_reason"
        case playbackBufferEmpty = "playback_buffer_empty"
        case playbackLikelyToKeepUp = "playback_likely_to_keep_up"
        case playbackBufferFull = "playback_buffer_full"
        case mediaRequests = "media_requests"
        case downloadedDuration = "downloaded_duration"
        case bytesTransferred = "bytes_transferred"
        case transferDuration = "transfer_duration"
        case observedBitrateBps = "observed_bitrate_bps"
        case indicatedBitrateBps = "indicated_bitrate_bps"
        case accessStalls = "access_stalls"
        case server
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
    let sessionId: String?
    let attempt: String
    let reason: String
    let snapshot: ApplePlaybackDiagnosticSnapshot
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
        encoder: String?,
        sessionId: String?,
        attempt: String,
        snapshot: ApplePlaybackDiagnosticSnapshot
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
        self.sessionId = sessionId
        self.attempt = attempt
        reason = "stall-\(kind.rawValue)"
        self.snapshot = snapshot
    }

    enum CodingKeys: String, CodingKey {
        case level, event, message, method, title, detail, ms, encoder, attempt, reason, snapshot, ua
        case fileId = "file_id"
        case sessionId = "session_id"
        case vcodec
    }
}

/// AVPlayer's own access-log counter catches shorter waits that recover before
/// the six-sample recovery ladder acts. It rides the ordinary progress cadence
/// and keeps the same bounded, token-free client-log shape as ladder beacons.
struct ApplePlaybackObservedStallLog: Encodable {
    let level = "warn"
    let event = "stall"
    let message = "AVPlayer reported a self-recovered stall"
    let method: String
    let title: String
    let fileId: Int
    let vcodec: String?
    let detail: String
    let ms: Int
    let encoder: String?
    let sessionId: String?
    let attempt: String
    let reason = "self-recovered"
    let snapshot: ApplePlaybackDiagnosticSnapshot
    let ua = "Apple AVPlayer"

    init(
        delta: Int,
        positionMs: Int,
        stagnantDurationMs: Int,
        method: String,
        title: String,
        fileId: Int,
        vcodec: String?,
        encoder: String?,
        sessionId: String?,
        attempt: String,
        snapshot: ApplePlaybackDiagnosticSnapshot
    ) {
        self.method = method
        self.title = title
        self.fileId = fileId
        self.vcodec = vcodec
        detail = "kind=access_log · position_ms=\(max(0, positionMs)) · stall_delta=\(max(1, delta)) · outcome=self_recovered"
        ms = max(0, stagnantDurationMs)
        self.encoder = encoder
        self.sessionId = sessionId
        self.attempt = attempt
        self.snapshot = snapshot
    }

    enum CodingKeys: String, CodingKey {
        case level, event, message, method, title, detail, ms, encoder, attempt, reason, snapshot, ua
        case fileId = "file_id"
        case sessionId = "session_id"
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

    /// Also advances the generation: a teardown must invalidate every seek
    /// still sleeping in its coalescing pause or awaiting AVPlayer, or a
    /// stop()-then-start() boundary (autoplay-next) lets a stale task fire
    /// into the next playback with the previous film's target.
    mutating func clear() {
        pendingMs = nil
        generation &+= 1
    }

    private static func clamp(_ requestedMs: Int, durationMs: Int) -> Int {
        let upper = durationMs > 0 ? max(0, durationMs - 2_000) : Int.max
        return min(max(0, requestedMs), upper)
    }
}

/// How an interactive seek reaches its target. `native` moves the current
/// item's own clock — instant, no server round trip; `reopen` replaces the
/// server session at the film position, which is the only way to reach media
/// the growing playlist has not published (or has already pruned).
enum PlayerSeekRoute: Equatable {
    case native(itemMs: Int)
    case reopen
}

enum PlaybackStallAction: Equatable {
    case none
    case nudge
    case reopen
}

enum PlaybackStallKind: String, Equatable {
    case silent
    case buffering
    /// The server's own delivery clock said this player stopped fetching
    /// published media (`delivered_idle_ms` grew while `published - fetched`
    /// stayed deep). Detected from the 2-second status poll, so it fires even
    /// when AVPlayer's `timeControlStatus` flaps or lies — the wedge observed
    /// on tvOS 2160p copy-HLS sessions that froze without ever tripping the
    /// position-clock ladder.
    case delivery

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
        case .delivery:
            return PlaybackStallTerminalState(
                message: "Playback stopped fetching media after retrying the current stream. Check the connection and try again."
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

/// Server-truth wedge detector, fed by the 2-second session-status poll.
///
/// The position-clock ladder can only see what AVPlayer admits. The failure
/// this catches is the one where AVPlayer admits nothing: it keeps reloading
/// the playlist, requests no media, and reports whatever `timeControlStatus`
/// it likes — observed live on tvOS with 250 s of published, fetchable media
/// waiting. The server already measures that wedge precisely
/// (`delivered_idle_ms` since the last completed media delivery, and how much
/// published media the client has not fetched), and this client already polls
/// those numbers every two seconds. Two consecutive qualifying polls fire one
/// recovery through the same bounded same-delivery ladder as every other
/// stall; ineligible states (paused, stream change in flight, seek pending,
/// nothing actually pending server-side) reset the confirmation count.
struct DeliveryStarvationDetector: Equatable {
    /// A healthy steady-state player completes a segment fetch every segment
    /// duration (~6-10 s here). Sixteen seconds of completed-delivery silence
    /// while media is pending is beyond any healthy cadence.
    static let deliveredIdleThresholdMs = 16_000
    /// Require a real backlog, so playing out the tail of a finished stream
    /// (published == fetched) can never qualify.
    static let pendingMediaThresholdMs = 10_000
    static let confirmationsRequired = 2

    private(set) var confirmations = 0

    mutating func observe(
        deliveredIdleMs: Int?,
        publishedEndMs: Int?,
        fetchedEndMs: Int?,
        eligible: Bool
    ) -> Bool {
        guard eligible,
              let deliveredIdleMs, deliveredIdleMs >= Self.deliveredIdleThresholdMs,
              let publishedEndMs,
              let fetchedEndMs,
              publishedEndMs - fetchedEndMs >= Self.pendingMediaThresholdMs
        else {
            confirmations = 0
            return false
        }
        confirmations += 1
        guard confirmations >= Self.confirmationsRequired else { return false }
        confirmations = 0
        return true
    }

    mutating func reset() { confirmations = 0 }
}

/// Global brake on automatic session replacements. Each recovery path is
/// individually bounded, but their budgets reset on different evidence, and
/// the early-end guard keys on a repeated *position* — a reopen that lands a
/// few seconds off each time evades it forever. The observed failure: nine
/// sessions opened in sixteen seconds, none reaching first frame. Whatever
/// the loop, the fourth automatic reopen inside a rolling minute stops
/// playback with the visible failure screen instead.
struct RecoveryReopenBudget: Equatable {
    static let windowSeconds: TimeInterval = 60
    static let maxAutomaticReopens = 3

    private(set) var reopenTimes: [TimeInterval] = []

    mutating func admit(
        at now: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) -> Bool {
        reopenTimes.removeAll { now - $0 > Self.windowSeconds }
        guard reopenTimes.count < Self.maxAutomaticReopens else { return false }
        reopenTimes.append(now)
        return true
    }

    mutating func reset() { reopenTimes.removeAll() }
}

/// Per-AVPlayerItem establishment gate. Every replacement begins unarmed and
/// has to advance five seconds on its own clock before buffering recovery may
/// treat a wait as a mid-playback interruption.
struct PlayerAttachmentRecoveryState: Equatable {
    private(set) var attachedAtPositionMs = 0
    private(set) var establishedPlayback = false

    mutating func opened(at positionMs: Int) {
        attachedAtPositionMs = max(0, positionMs)
        establishedPlayback = false
    }

    mutating func observe(positionMs: Int, playing: Bool) {
        guard playing,
              positionMs >= attachedAtPositionMs + 5_000
        else { return }
        establishedPlayback = true
    }
}

/// A decoder that accepts the stream and renders nothing is invisible to every
/// other detector here: audio advances the film clock, so both stall detectors
/// reset on each sample, AVPlayer's stall counter never moves, and no NSError
/// is ever produced. The one piece of evidence is `AVPlayerItem.presentationSize`,
/// which stays `.zero` until a frame has actually been decoded. Dolby Vision
/// Profile 5 on a device with no Profile 5 decoder fails exactly this way.
///
/// This deliberately does not reuse `PlayerAttachmentRecoveryState`: that gate
/// arms after five seconds of clock progress whether or not a picture ever
/// appeared, which is *below* this threshold, so it would disarm the watchdog
/// before it could ever fire. A rendered frame is what establishes video here,
/// and the first non-zero presentation size retires the watchdog for good.
struct BlackFrameWatchdog: Equatable {
    private(set) var lastPositionMs: Int?
    private(set) var blackMs = 0
    private(set) var presentedVideo = false
    private(set) var fired = false

    mutating func opened() {
        lastPositionMs = nil
        blackMs = 0
        presentedVideo = false
        fired = false
    }

    /// True exactly once, on the sample that proves the film clock ran for
    /// `PlayerController.blackFrameDecodeFailureMs` with nothing on screen.
    ///
    /// `hasVideoSource` keeps audiobooks and other audio-only playbacks out:
    /// their presentation size is legitimately `.zero` forever.
    @MainActor
    mutating func observe(
        positionMs: Int,
        presentationSize: CGSize,
        hasVideoSource: Bool,
        playing: Bool
    ) -> Bool {
        guard !fired, !presentedVideo else { return false }
        if presentationSize.width > 0 && presentationSize.height > 0 {
            presentedVideo = true
            lastPositionMs = nil
            blackMs = 0
            return false
        }
        guard hasVideoSource, playing else {
            lastPositionMs = nil
            return false
        }
        guard let lastPositionMs else {
            self.lastPositionMs = positionMs
            return false
        }
        let delta = positionMs - lastPositionMs
        self.lastPositionMs = positionMs
        // Only elapsed film time counts. A seek, an item replacement, or any
        // other discontinuity larger than one sampling interval can explain is
        // a new baseline rather than more of the same black screen.
        guard delta >= 0, delta <= PlayerController.blackFrameSampleCeilingMs else {
            blackMs = 0
            return false
        }
        blackMs += delta
        guard blackMs >= PlayerController.blackFrameDecodeFailureMs else { return false }
        fired = true
        return true
    }
}

struct PlaybackStallObservation: Equatable {
    let delta: Int
    let stagnantDurationMs: Int
}

/// Deltas AVPlayer's cumulative access-log counter and holds the detector's
/// latest sub-threshold stagnant interval until the next 10-second progress
/// report. A new item resets the counter because AVPlayer does too.
struct PlaybackStallObservationState: Equatable {
    private var lastNumberOfStalls = 0
    private var pendingStagnantDurationMs = 0

    mutating func noteRecoveredStagnation(_ durationMs: Int?) {
        pendingStagnantDurationMs = max(pendingStagnantDurationMs, durationMs ?? 0)
    }

    mutating func take(numberOfStalls: Int?) -> PlaybackStallObservation? {
        guard let numberOfStalls else { return nil }
        let current = max(0, numberOfStalls)
        let delta = current >= lastNumberOfStalls
            ? current - lastNumberOfStalls
            : current
        lastNumberOfStalls = current
        defer { pendingStagnantDurationMs = 0 }
        guard delta > 0 else { return nil }
        return PlaybackStallObservation(
            delta: delta,
            stagnantDurationMs: pendingStagnantDurationMs
        )
    }

    mutating func reset() {
        lastNumberOfStalls = 0
        pendingStagnantDurationMs = 0
    }
}

enum PlaybackCompatibilityFallback: Equatable {
    case none
    case hdrBase
    case transcode

    /// Names the rung in the client failure log, so a device log says which
    /// recovery a failure actually bought rather than only that it happened.
    var telemetryName: String {
        switch self {
        case .none: return "none"
        case .hdrBase: return "hdr-base"
        case .transcode: return "transcode"
        }
    }
}

/// What put a playback into the compatibility ladder. Only `itemFailure`
/// carries an AVFoundation error; the other two are client-observed verdicts
/// with no NSError to print, which is exactly why they need naming.
enum PlaybackCompatibilityLadderCause: String, Equatable {
    case itemFailure = "item-failed"
    case readinessTimeout = "readiness-timeout"
    case blackFrames = "black-frames"
}

/// One ladder decision, recorded in `ApplePlaybackFailureLog.detail`.
struct PlaybackCompatibilityLadderStep: Equatable {
    let cause: PlaybackCompatibilityLadderCause
    let fallback: PlaybackCompatibilityFallback
}

/// Monotonic elapsed-time sampling policy for AVPlayer's silent-wait failure mode. A
/// temporary buffer wait gets room to recover on its own; only sustained lack
/// of film-time progress rebuilds the item, which is the in-player equivalent
/// of the back-out-and-play-again workaround.
///
/// One shared clock counts stagnation regardless of `timeControlStatus`. The
/// previous design ran a separate detector per regime and zeroed each one
/// whenever AVPlayer crossed between `.playing` and
/// `.waitingToPlayAtSpecifiedRate`; a starving 4K session flaps between those
/// faster than either detector's threshold, so a real freeze never latched
/// (observed on tvOS: sessions died server-side as `idle` with no stall beacon
/// ever sent). The regime now only *labels* the eventual event — via a tally
/// majority — instead of gating the count.
struct PlaybackStallDetector: Equatable {
    /// Consecutive stagnant 2-second samples before a nudge / a reopen, once
    /// this item has established playback (5 s of real progress).
    static let establishedNudgeChecks = 3
    static let establishedReopenChecks = 6
    /// An unestablished item — the first attach, or the item a recovery
    /// reopen just created — may legitimately buffer for a while before its
    /// first frame, so it gets a longer leash and no nudge. Before this
    /// threshold existed the unestablished state had NO detector at all: a
    /// reopen that landed into continued starvation froze forever with no
    /// error. Fifteen checks ≈ 30 s, then the ladder decides (one bounded
    /// reopen, then the visible terminal screen).
    static let unestablishedReopenChecks = 15

    private(set) var lastPositionMs: Int?
    private(set) var stagnantChecks = 0
    private(set) var stagnantSince: TimeInterval?
    private(set) var recoveredDurationMs: Int?
    /// How many of the current stagnation's samples were taken while AVPlayer
    /// reported `.waitingToPlayAtSpecifiedRate`. Majority picks the event
    /// kind: mostly-waiting → `.buffering` (transport recovery only), so a
    /// flapping network stall can never be misread as decoder evidence and
    /// spend the codec/HDR compatibility ladder.
    private(set) var waitingSamples = 0

    /// The regime majority of the stagnation run that produced the most
    /// recent `.reopen`, snapshotted at fire time because firing restarts
    /// the counters.
    private(set) var firedWaitingMajority = false

    /// Ties go to `.buffering`: an ambiguous stagnation gets transport
    /// recovery, never the codec/HDR ladder. Only a run that was mostly
    /// "playing" — the clock stopped while the player claimed motion — may
    /// count as decoder evidence.
    static func waitingMajority(waitingSamples: Int, stagnantChecks: Int) -> Bool {
        waitingSamples * 2 >= stagnantChecks
    }

    mutating func sample(
        positionMs: Int,
        shouldMonitor: Bool,
        established: Bool,
        waitingRegime: Bool,
        observedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) -> PlaybackStallAction {
        guard shouldMonitor else {
            recordRecovery(at: observedAt)
            clearSample()
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
            recordRecovery(at: observedAt)
            self.lastPositionMs = positionMs
            stagnantChecks = 0
            stagnantSince = observedAt
            waitingSamples = 0
            return .none
        }

        stagnantChecks += 1
        if waitingRegime { waitingSamples += 1 }
        let reopenChecks = established
            ? Self.establishedReopenChecks
            : Self.unestablishedReopenChecks
        if stagnantChecks >= reopenChecks {
            firedWaitingMajority = Self.waitingMajority(
                waitingSamples: waitingSamples, stagnantChecks: stagnantChecks
            )
            self.lastPositionMs = positionMs
            stagnantChecks = 0
            waitingSamples = 0
            return .reopen
        }
        if established && stagnantChecks == Self.establishedNudgeChecks {
            firedWaitingMajority = Self.waitingMajority(
                waitingSamples: waitingSamples, stagnantChecks: stagnantChecks
            )
            return .nudge
        }
        return .none
    }

    func stagnantDurationMs(at observedAt: TimeInterval) -> Int {
        guard let stagnantSince else { return 0 }
        return max(0, Int(((observedAt - stagnantSince) * 1_000).rounded()))
    }

    mutating func takeRecoveredDurationMs() -> Int? {
        defer { recoveredDurationMs = nil }
        return recoveredDurationMs
    }

    private mutating func recordRecovery(at observedAt: TimeInterval) {
        guard stagnantChecks > 0 else { return }
        recoveredDurationMs = max(
            recoveredDurationMs ?? 0,
            stagnantDurationMs(at: observedAt)
        )
    }

    private mutating func clearSample() {
        lastPositionMs = nil
        stagnantChecks = 0
        stagnantSince = nil
        waitingSamples = 0
    }

    mutating func reset() {
        clearSample()
        recoveredDurationMs = nil
        firedWaitingMajority = false
    }
}

/// Owns the shared stall clock so predicate gating, kind labeling, and
/// measured duration are one testable policy rather than parallel expressions
/// inside an asynchronous AVPlayer loop.
///
/// One detector, not one per regime: `timeControlStatus` crossing between
/// `.playing` and `.waitingToPlayAtSpecifiedRate` must never restart the
/// count (the flap itself is a symptom of the stall being measured). The
/// regime tally only decides the event's kind — mostly-waiting stagnation is
/// `.buffering` and stays inside transport recovery; only a stagnation that
/// was mostly "playing" (the clock stopped while the player claimed motion)
/// counts as `.silent`, the decoder-evidence path.
struct PlaybackRecoveryMonitor: Equatable {
    private(set) var progressDetector = PlaybackStallDetector()
    private var recoveredStagnantDurationMs: Int?

    @MainActor
    mutating func sample(
        positionMs: Int,
        timeControlStatus: AVPlayer.TimeControlStatus,
        shouldMonitor: Bool,
        establishedPlayback: Bool,
        observedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    ) -> PlaybackStallEvent? {
        let action = progressDetector.sample(
            positionMs: positionMs,
            shouldMonitor: shouldMonitor,
            established: establishedPlayback,
            waitingRegime: PlayerController.shouldMonitorBufferingStall(
                timeControlStatus: timeControlStatus
            ),
            observedAt: observedAt
        )
        if let recovered = progressDetector.takeRecoveredDurationMs() {
            recoveredStagnantDurationMs = max(recoveredStagnantDurationMs ?? 0, recovered)
        }
        guard action != .none else { return nil }
        // `stagnantSince` survives a fire (only real progress or a gate change
        // clears it), so this duration describes the whole run that fired.
        return PlaybackStallEvent(
            kind: progressDetector.firedWaitingMajority ? .buffering : .silent,
            action: action,
            positionMs: positionMs,
            durationMs: progressDetector.stagnantDurationMs(at: observedAt)
        )
    }

    mutating func takeRecoveredStagnantDurationMs() -> Int? {
        defer { recoveredStagnantDurationMs = nil }
        return recoveredStagnantDurationMs
    }

    mutating func reset() {
        progressDetector.reset()
        recoveredStagnantDurationMs = nil
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

enum PlayerItemEndAction: Equatable {
    case reopen
    case stop
    case finish(durationMs: Int)
}

enum SameDeliveryRecoveryTransport: Equatable {
    case serverSession
    case offlineAsset
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
    /// The server retains 180 seconds behind the download frontier: the 120 s
    /// forward lead measured on a physical iPad, 30 s for back-buffering, and
    /// 30 s for retry/reload. `preferredForwardBufferDuration` is a hint, not a
    /// cap — AVPlayer fetched twice this 60 s preference. Keep growing HLS
    /// sessions inside that contract while leaving direct and completed-VOD
    /// items under AVPlayer's normal policy.
    static let growingHLSForwardBufferSeconds: TimeInterval = 60
    static let repeatedEndToleranceMs = 250
    static let naturalEndToleranceMs = 15_000
    /// How long an attached item may sit at `.unknown` before the wait is
    /// spent. A resume has always been bounded by this because it had to seek;
    /// a fresh start was not bounded at all, which is what let a title AVPlayer
    /// neither readies nor fails hold the open forever behind a black screen.
    static let itemReadinessDeadlineSeconds = 15
    /// Film time that may pass on a black screen before the picture is judged
    /// undecodable. Above `PlayerAttachmentRecoveryState`'s five-second
    /// establishment window on purpose: audio-only progress must not be able
    /// to certify a playback that has never drawn a frame.
    static let blackFrameDecodeFailureMs = 6_000
    /// The most film time one periodic sample may contribute to that total.
    /// The observer runs twice a second, so anything past this is a seek or an
    /// item replacement, not playback — and must not be counted as black.
    static let blackFrameSampleCeilingMs = 2_000
    static let gracefulRepeatedEndFraction = 0.95
    static let playbackStartFailureTitle = "Couldn't start playback."
    static let playbackStoppedFailureTitle = "Playback stopped."
    static let earlyEndFailureTitle = "Playback stopped early."
    static let repeatedEarlyEndMessage =
        "Playback ended early at the same position after retrying the stream."
    static let readinessTimeoutMessage =
        "The stream never became ready to play."
    static let blackFrameFailureMessage =
        "The film clock advanced with no decoded picture."

    /// A fresh start hands `seekWhenReady` nothing to wait on, so before this
    /// only a resume was bounded. Bound the completed-VOD/direct cold start the
    /// same way.
    ///
    /// Two exclusions, both because the wait is then somebody else's: a growing
    /// session may still be filling the server's publish gate, and a reopen
    /// that lands paused is not a viewer sitting in front of a black screen —
    /// it is an item deliberately prepared at rate 0.
    nonisolated static func shouldBoundFreshStartReadiness(
        isVOD: Bool,
        startMs: Int,
        seeksAfterAttach: Bool,
        resumesPlayback: Bool
    ) -> Bool {
        isVOD && startMs <= 0 && !seeksAfterAttach && resumesPlayback
    }

    static func configureBuffering(_ item: AVPlayerItem, growingHLS: Bool) {
        item.preferredForwardBufferDuration = growingHLS
            ? growingHLSForwardBufferSeconds
            : 0
    }

    /// AVPlayer can announce the temporary end of a growing playlist. A
    /// server duration proves where the title should end; a finite item
    /// duration is a safe fallback for direct/offline files whose catalog row
    /// was never probed. One early end may reopen, but the replacement ending
    /// at the same clock position is terminal: reopening it again cannot make
    /// progress and otherwise creates an unbounded session loop.
    nonisolated static func endAction(
        knownDurationMs: Int,
        itemDurationMs: Int?,
        isGrowingPlaylist: Bool,
        endedAt: Int,
        previousUncorroboratedEndMs: Int? = nil
    ) -> PlayerItemEndAction {
        let corroboratedDuration = knownDurationMs > 0
            ? knownDurationMs
            : isGrowingPlaylist
                ? nil
                : itemDurationMs.flatMap { $0 > 0 ? $0 : nil }
        let repeatedAtSamePosition = previousUncorroboratedEndMs
            .map {
                abs(max(0, endedAt) - max(0, $0)) < repeatedEndToleranceMs
            }
            ?? false
        guard let durationMs = corroboratedDuration else {
            guard repeatedAtSamePosition else { return .reopen }
            if isGrowingPlaylist { return .stop }
            return .finish(durationMs: max(endedAt, previousUncorroboratedEndMs ?? 0))
        }
        guard endedAt >= durationMs - naturalEndToleranceMs else {
            guard repeatedAtSamePosition else { return .reopen }
            let terminalPosition = max(endedAt, previousUncorroboratedEndMs ?? 0)
            if Double(terminalPosition) / Double(durationMs) >= gracefulRepeatedEndFraction {
                return .finish(durationMs: terminalPosition)
            }
            return .stop
        }
        return .finish(durationMs: durationMs)
    }

    let player = AVPlayer()

    @Published private(set) var decision: Decision?
    @Published private(set) var sessionStatus: PlaybackSessionStatus?
    /// Last successful status response for a stall report. The visible status
    /// is allowed to become unavailable when a poll fails, but that failure is
    /// precisely when diagnostics must retain the last server evidence and say
    /// how old it was.
    private var diagnosticSessionStatus: PlaybackSessionStatus?
    private var diagnosticSessionStatusObservedAt: Date?
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
    @Published private(set) var playbackFailureTitle = PlayerController.playbackStartFailureTitle
    @Published private(set) var playbackNotice: String?
    @Published private(set) var finished = false
    @Published private(set) var pgsOverlayWindow: PGSOverlayWindow?
    @Published private(set) var pgsOverlayStatus: PGSOverlayStatus = .off

    private var baseMs = 0
    private var itemId = 0
    private var fileId = 0
    private var progressOffsetMs = 0
    private var itemDurationMs: Int?
    private var title = ""
    #if os(iOS)
    private var offlineId: String?
    private var offlineAssetURL: URL?
    #endif
    private var audioOverride: Int?
    /// The detail screen's pre-play choice for *this* playback. It travels to
    /// `/decision`, so the server's verdict and delivery plan already account
    /// for it — which is exactly why it is not an `audioOverride`: overriding
    /// would force a copy session onto a choice the server judged direct-
    /// playable, downgrading delivery past what the verdict states.
    private var prePlaySelection = PrePlaySelection.none
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
    private var attachmentRecovery = PlayerAttachmentRecoveryState()
    /// The only evidence a decoder is producing nothing while the film clock
    /// runs. Scoped to one attached item, exactly like `attachmentRecovery`.
    private var blackFrameWatchdog = BlackFrameWatchdog()
    private var establishedHDRRetryAttempted = false
    private var ttffMeasurement = ApplePlaybackTTFFState()
    private var ttffReason = "cold-start"
    private var stallObservation = PlaybackStallObservationState()
    /// An item that ends before its expected boundary gets one reopen. A
    /// second end at the same position finishes an otherwise uncorroborated
    /// local item, or stops a growing/known-duration stream visibly, instead
    /// of creating an unbounded session loop.
    private(set) var lastUncorroboratedEndMs: Int?
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
    private var deliveryStarvation = DeliveryStarvationDetector()
    private var recoveryReopenBudget = RecoveryReopenBudget()
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
    private(set) var wantsPlayback = true
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
    /// Fresh for every attached AVPlayerItem. `playbackId` cannot serve this
    /// purpose because it deliberately stays stable across session reopens;
    /// telemetry needs the opposite so the stalled predecessor and recovered
    /// successor never collapse into one attempt.
    private var playbackAttemptId = UUID().uuidString
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
        let overlay = pgsOverlayIsActive ? " · PGS overlay" : ""
        switch mode {
        case "direct": return "Direct play\(overlay)"
        case "remux": return "Remux · HLS\(overlay)"
        default: return (isVOD ? "Transcode · cached" : "Transcode") + overlay
        }
    }

    private var clientLogMethod: String {
        isDirectPlayback ? "direct_play" : (encoder == "copy" ? "remux" : "transcode")
    }

    nonisolated static func isPGSOverlayActive(
        overlayTrackIndex: Int?,
        selectedSubtitle: Int?
    ) -> Bool {
        guard let overlayTrackIndex else { return false }
        return overlayTrackIndex == selectedSubtitle
    }

    var pgsOverlayIsActive: Bool {
        Self.isPGSOverlayActive(
            overlayTrackIndex: pgsOverlayTrackIndex,
            selectedSubtitle: selectedSubtitle
        )
    }

    var observedBitrate: Double? {
        player.currentItem?.accessLog()?.events.last?.observedBitrate
    }

    var indicatedBitrate: Double? {
        player.currentItem?.accessLog()?.events.last?.indicatedBitrate
    }

    var stalls: Int? {
        player.currentItem?.accessLog()?.events.last?.numberOfStalls
    }

    /// Playable seconds contiguous with the current clock. A later buffered
    /// island beyond a gap is not runway: AVPlayer still has to stop before it
    /// can reach it.
    nonisolated static func bufferedRunwaySeconds(
        playheadSeconds: Double,
        ranges: [ClosedRange<Double>],
        joinTolerance: Double = 0.25
    ) -> Double {
        guard playheadSeconds.isFinite else { return 0 }
        let ranges = ranges
            .filter {
                $0.lowerBound.isFinite
                    && $0.upperBound.isFinite
                    && $0.upperBound >= $0.lowerBound
            }
            .sorted { $0.lowerBound < $1.lowerBound }
        guard let start = ranges.firstIndex(where: {
            $0.lowerBound <= playheadSeconds + joinTolerance
                && $0.upperBound >= playheadSeconds - joinTolerance
        }) else { return 0 }
        var end = ranges[start].upperBound
        for range in ranges.dropFirst(start + 1) {
            guard range.lowerBound <= end + joinTolerance else { break }
            end = max(end, range.upperBound)
        }
        return max(0, end - playheadSeconds)
    }

    private func playbackDiagnosticSnapshot(at positionMs: Int) -> ApplePlaybackDiagnosticSnapshot {
        var snapshot = ApplePlaybackDiagnosticSnapshot()
        snapshot.positionMs = max(0, positionMs)
        snapshot.timeControlStatus = Self.timeControlStatusLabel(player.timeControlStatus)
        snapshot.waitingReason = player.reasonForWaitingToPlay?.rawValue
        if let item = player.currentItem {
            let localSeconds = item.currentTime().seconds
            let loaded = item.loadedTimeRanges.compactMap { value -> ClosedRange<Double>? in
                let range = value.timeRangeValue
                let start = range.start.seconds
                let end = CMTimeRangeGetEnd(range).seconds
                guard start.isFinite, end.isFinite, end >= start else { return nil }
                return start...end
            }
            snapshot.runway = Self.bufferedRunwaySeconds(
                playheadSeconds: localSeconds,
                ranges: loaded
            )
            snapshot.playbackBufferEmpty = item.isPlaybackBufferEmpty
            snapshot.playbackLikelyToKeepUp = item.isPlaybackLikelyToKeepUp
            snapshot.playbackBufferFull = item.isPlaybackBufferFull
            if let event = item.accessLog()?.events.last {
                snapshot.mediaRequests = Self.nonnegative(event.numberOfMediaRequests)
                snapshot.downloadedDuration = Self.nonnegative(event.segmentsDownloadedDuration)
                snapshot.bytesTransferred = event.numberOfBytesTransferred >= 0
                    ? Int(clamping: event.numberOfBytesTransferred)
                    : nil
                snapshot.transferDuration = Self.nonnegative(event.transferDuration)
                snapshot.observedBitrateBps = Self.nonnegative(event.observedBitrate)
                snapshot.indicatedBitrateBps = Self.nonnegative(event.indicatedBitrate)
                snapshot.accessStalls = Self.nonnegative(event.numberOfStalls)
            }
        }
        if let status = diagnosticSessionStatus {
            let ageMs = diagnosticSessionStatusObservedAt.map {
                max(0, Int(Date().timeIntervalSince($0) * 1_000))
            }
            snapshot.server = ApplePlaybackServerSnapshot(status, observedAgeMs: ageMs)
        }
        return snapshot
    }

    nonisolated private static func nonnegative<T: BinaryInteger>(_ value: T) -> Int? {
        value >= 0 ? Int(clamping: value) : nil
    }

    nonisolated private static func nonnegative(_ value: Double) -> Double? {
        value.isFinite && value >= 0 ? value : nil
    }

    nonisolated static func timeControlStatusLabel(_ status: AVPlayer.TimeControlStatus) -> String {
        switch status {
        case .paused: return "paused"
        case .waitingToPlayAtSpecifiedRate: return "waiting"
        case .playing: return "playing"
        @unknown default: return "unknown"
        }
    }

    var presentationSize: CGSize { player.currentItem?.presentationSize ?? .zero }

    func start(
        model: AppModel,
        itemId: Int,
        fileId: Int,
        startMs: Int,
        durationMs: Int,
        progressOffsetMs: Int = 0,
        itemDurationMs: Int? = nil,
        title: String,
        selection: PrePlaySelection = .none
    ) {
        guard !started else { return }
        started = true
        #if os(iOS)
        offlineId = nil
        offlineAssetURL = nil
        #endif
        self.model = model
        self.itemId = itemId
        self.fileId = fileId
        self.progressOffsetMs = max(0, progressOffsetMs)
        self.itemDurationMs = itemDurationMs
        self.knownDurationMs = durationMs
        self.currentMs = max(0, startMs)
        self.title = title
        self.prePlaySelection = selection
        audioOverride = nil
        finished = false
        playbackFailureTitle = Self.playbackStartFailureTitle
        subtitleReadiness = model.subtitleReadiness
        wantsNativeSubtitleRenditions = false
        canRetryCurrentItemWithHDRBase = false
        dolbyVisionFallbackAttempted = false
        forceCompatibleHDRBase = false
        canRetryCurrentItemWithTranscode = false
        compatibilityFallbackAttempted = false
        forceCompatibilityTranscode = false
        ttffReason = currentMs > 0 ? "resume" : "cold-start"
        ttffMeasurement.opened(at: currentMs)
        attachmentRecovery.opened(at: startMs)
        blackFrameWatchdog.opened()
        establishedHDRRetryAttempted = false
        stallObservation.reset()
        lastUncorroboratedEndMs = nil
        sameDeliveryStallRecovery.reset()
        deliveryStarvation.reset()
        recoveryReopenBudget.reset()
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
        offlineAssetURL = OfflineCatalog.localURL(for: path)
        itemId = offline.itemId
        fileId = offline.fileId
        knownDurationMs = offline.durationMs ?? 0
        currentMs = max(0, offline.positionMs)
        title = offline.title
        finished = false
        playbackFailureTitle = Self.playbackStartFailureTitle
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
        attachmentRecovery.opened(at: currentMs)
        blackFrameWatchdog.opened()
        stallObservation.reset()
        deliveryStarvation.reset()
        recoveryReopenBudget.reset()
        lastUncorroboratedEndMs = nil
        player.appliesMediaSelectionCriteriaAutomatically = false
        player.automaticallyWaitsToMinimizeStalling = true
        try? AVAudioSession.sharedInstance().setCategory(.playback)
        try? AVAudioSession.sharedInstance().setActive(true)
        installRemoteCommands()
        addPeriodicObserver()
        startPlaybackRecoveryMonitor()
        Task { await loadOffline(url: OfflineCatalog.localURL(for: path), startMs: currentMs) }
    }

    nonisolated static func offlineDecision(_ item: OfflineItem) -> Decision {
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
        guard started else { return }
        let attemptId = UUID().uuidString
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
        playbackAttemptId = attemptId
        baseMs = 0
        player.play()
        if startMs > 0 {
            do { try await seekWhenReady(item, ms: startMs) }
            catch {
                guard started, player.currentItem === item else { return }
                fail(error)
                return
            }
        }
        guard started, player.currentItem === item else { return }
        await applyPreferredAudioSelection(to: item)
        await applyNativeSubtitleSelection(selectedSubtitle, to: item)
        guard started, player.currentItem === item else { return }
        player.play()
        isPlaying = true
        failed = false
        attachmentRecovery.opened(at: startMs)
        blackFrameWatchdog.opened()
        updateNowPlaying()
    }

    private func reloadOffline(at positionMs: Int) async {
        guard let offlineAssetURL, started else { return }
        isChangingStream = true
        playbackRecoveryMonitor.reset()
        deliveryStarvation.reset()
        await loadOffline(url: offlineAssetURL, startMs: positionMs)
        isChangingStream = false
        // The status observer holds its fire while `isChangingStream` is up;
        // a replacement that failed during its own load is caught here, the
        // same re-check `open()` performs for server sessions. `loadOffline`'s
        // own error paths have already surfaced through `fail()` — only a
        // suppressed KVO failure needs the second look.
        if !failed, let item = player.currentItem, item.status == .failed {
            await handleItemFailure(item)
        }
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

    /// A terminal automatic retry stays terminal until the viewer explicitly
    /// asks for another attempt. This keeps a malformed boundary from becoming
    /// an automatic loop while still offering recovery without dismissing the
    /// player.
    func retryAfterPlaybackFailure() {
        guard started, failed, decision != nil else { return }
        let position = currentMs
        failed = false
        playbackError = nil
        playbackFailureTitle = Self.playbackStartFailureTitle
        lastUncorroboratedEndMs = nil
        // The viewer explicitly asked for another attempt; the automatic
        // brake must not carry a spent window into it.
        recoveryReopenBudget.reset()
        wantsPlayback = true
        #if os(iOS)
        if offlineAssetURL != nil {
            Task { await reloadOffline(at: position) }
            return
        }
        #endif
        Task { await reopen(at: position) }
    }

    var canRetryPlaybackFailure: Bool {
        started && decision != nil
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
        ttffMeasurement.rebasePosition(at: target)
        refreshPGSOverlayWindow(at: target, force: true)
        playbackRecoveryMonitor.reset()
        deliveryStarvation.reset()
        let route = Self.seekRoute(
            targetMs: target,
            baseMs: baseMs,
            usesDirectTimeline: usesDirectTimeline,
            isVOD: isVOD,
            isChangingStream: isChangingStream,
            seekableRangesMs: currentItemSeekableRangesMs()
        )
        Task {
            switch route {
            case .native(let itemMs):
                _ = await player.seek(
                    to: CMTime(seconds: Double(itemMs) / 1000.0, preferredTimescale: 600),
                    toleranceBefore: .zero,
                    toleranceAfter: .zero
                )
                // Only the newest seek may publish or escalate; an older
                // completion arriving after AVPlayer cancelled it must not.
                guard generation == seekState.generation else { return }
                let landed = realPositionMs()
                // A growing window can go stale between the route decision
                // and the landing — AVPlayer then clamps somewhere else
                // entirely. The reopen path is the truth for that target;
                // `pendingMs` still names it, so the bar holds the target
                // and the reopen drain completes it.
                if !usesDirectTimeline, !isVOD, abs(landed - target) > 5_000 {
                    await reopen(at: target)
                    return
                }
                guard seekState.complete(generation: generation) else { return }
                currentMs = landed
                updateNowPlaying()
            case .reopen:
                // Coalesce a burst of out-of-window commands (remote-mash,
                // repeated scrubs) into the single newest reopen instead of
                // one server session per press. The optimistic target is
                // already on screen; only the newest generation survives
                // the pause. Track/quality changes never pass through here,
                // so their reopens stay immediate.
                try? await Task.sleep(for: .milliseconds(350))
                guard generation == seekState.generation else { return }
                await reopen(at: target)
            }
        }
    }

    /// The current item's seekable windows, in its own local clock. Empty
    /// while no item is attached or the playlist has not loaded yet — both
    /// route a seek to the reopen path.
    private func currentItemSeekableRangesMs() -> [ClosedRange<Int>] {
        guard let item = player.currentItem else { return [] }
        return item.seekableTimeRanges.compactMap { value in
            let range = value.timeRangeValue
            let start = range.start.seconds
            let duration = range.duration.seconds
            guard start.isFinite, duration.isFinite, duration > 0 else { return nil }
            let lower = Int(start * 1000)
            let upper = Int((start + duration) * 1000)
            guard upper > lower else { return nil }
            return lower...upper
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
    func stop(deactivateAudioSession: Bool = true) {
        clearPlaybackNotice()
        let wasStarted = started
        started = false
        // Invalidate every in-flight open before any of its awaits can attach
        // a replacement item after this teardown.
        openGeneration &+= 1
        reopenQueue.clear()
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
        pgsOverlayItemGeneration &+= 1
        playbackRecoveryMonitor.reset()
        deliveryStarvation.reset()
        ttffMeasurement.reset()
        seekState.clear()
        let position = realPositionMs()
        player.pause()
        player.replaceCurrentItem(with: nil)
        isPlaying = false
        wantsPlayback = false
        isChangingStream = false
        finished = false
        sessionStatus = nil
        diagnosticSessionStatus = nil
        diagnosticSessionStatusObservedAt = nil
        if wasStarted { report(position) }
        if let sessionId {
            self.sessionId = nil
            let model = model
            Task { await model?.endHlsSession(sessionId) }
        }
        #if os(iOS)
        offlineId = nil
        offlineAssetURL = nil
        if wasStarted {
            removeRemoteCommands()
            MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
            MPNowPlayingInfoCenter.default().playbackState = .stopped
            if deactivateAudioSession {
                try? AVAudioSession.sharedInstance().setActive(
                    false,
                    options: .notifyOthersOnDeactivation
                )
            }
        }
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
            let decision = try await model.decision(
                fileId: fileId,
                selection: prePlaySelection
            )
            guard started else { return }
            self.decision = decision
            if knownDurationMs <= 0 { knownDurationMs = decision.source?.durationMs ?? 0 }
            // `default` on a decision track is the server's own shared-policy
            // pick, not the muxer's flag (crates/plurxd http/stream.rs
            // overwrites both lists from `select_tracks`) — and for a
            // selection-aware request it is the *effective* pick, so a pre-play
            // audio choice already arrives marked here.
            selectedAudio = decision.audio?.first(where: { $0.default })?.index
            let wantedSubtitle = Self.initialSubtitleIndex(
                prePlay: prePlaySelection.subtitleIndex,
                serverSelection: decision.selection,
                tracks: decision.subtitles ?? [],
                deviceSubtitlesOff: model.subLang == "off"
            )
            let blockedByHDR = Self.startingSubtitleBlockedByHDR(
                prePlaySubtitle: prePlaySelection.subtitleIndex,
                wanted: wantedSubtitle,
                serverSelection: decision.selection,
                tracks: decision.subtitles ?? [],
                deliveredRange: decision.deliveredDynamicRange
            )
            selectedSubtitle = blockedByHDR ? nil : wantedSubtitle
            // Say so rather than starting silently with no subtitles: the
            // viewer asked for these on the detail screen.
            if blockedByHDR, prePlaySelection.subtitleIndex != nil {
                showPlaybackNotice(Self.hdrSubtitleNotice)
            }
            // A native pre-play choice enters the session on this very open, so
            // record it the way `selectSubtitle` does — otherwise turning
            // subtitles off later would drop back to direct play and charge the
            // next selection a restart this path already paid for. Scoped to
            // the pre-play arm so the automatic path keeps its behaviour.
            if prePlaySelection.subtitleIndex != nil,
               let selectedSubtitle,
               !Self.subtitleRequiresBurn(selectedSubtitle, in: decision.subtitles ?? []) {
                wantsNativeSubtitleRenditions = true
            }
            updatePGSOverlaySelection(selectedSubtitle)
            // Use the same drain as later replacements. A remote command can
            // arrive while the decision request or first item is preparing;
            // opening directly here left that command stranded in the reopen
            // queue forever. A command issued before the decision arrived is
            // already represented by `seekState.pendingMs`.
            let initialPosition = seekState.pendingMs ?? startMs
            try await openAndDrain(decision: decision, at: initialPosition)
        } catch {
            guard started else { return }
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
                // Parity with `stop()`: nothing will complete a pending
                // target after the viewer has left, and a stale one would
                // freeze `currentMs` on the next playback of this view.
                seekState.clear()
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
        let attemptId = UUID().uuidString
        finished = false
        attachmentRecovery.opened(at: startMs)
        blackFrameWatchdog.opened()
        isChangingStream = true
        failed = false
        playbackError = nil
        playbackFailureTitle = Self.playbackStartFailureTitle
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
                plan: decision.delivery?.audio,
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
            } else if let itemPosition = Self.sessionAttachSeekMs(
                requestedStartMs: startMs,
                mediaOriginMs: nextBaseMs
            ) {
                // A copy session begins on the preceding keyframe. Timeline
                // mapping alone makes progress truthful but still replays that
                // lead-in after a stall recovery; seek within the newly
                // published item so the first rendered frame lands back at the
                // requested film position.
                seekAfterAttach = itemPosition
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
        stallObservation.reset()
        player.replaceCurrentItem(with: item)
        playbackAttemptId = attemptId
        // Publish the new local-to-film mapping only once the new item is the
        // one whose clock `realPositionMs()` reads. Updating it during session
        // creation mixed the predecessor's local time into the successor's
        // base and was the source of the apparently random seek jumps.
        baseMs = nextBaseMs
        if seekAfterAttach == nil {
            ttffMeasurement.rebasePosition(at: realPositionMs())
        }
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
                ttffMeasurement.rebasePosition(at: realPositionMs())
            } catch {
                if isSuperseded(generation) { return }
                // A failed replacement surfaces here on the VOD/direct resume
                // path before the post-open re-check below can run, and its
                // status observer fired into the change-suppression window.
                // Route it through the same ladder instead of a bare failure,
                // or the compatibility fallbacks are silently lost.
                if item.status == .failed {
                    isChangingStream = false
                    await handleItemFailure(item)
                    return
                }
                throw error
            }
        } else if Self.shouldBoundFreshStartReadiness(
            isVOD: isVOD,
            startMs: startMs,
            seeksAfterAttach: seekAfterAttach != nil,
            resumesPlayback: resumesPlayback
        ) {
            // A fresh start has no seek to wait behind, so nothing used to
            // bound its wait for a first frame at all. An item AVFoundation
            // neither readies nor fails — Dolby Vision Profile 5 on a device
            // that cannot decode it — held this open forever, which is the
            // black screen viewers reported.
            do {
                try await awaitItemReady(item)
            } catch {
                if isSuperseded(generation) { return }
                if item.status == .failed {
                    isChangingStream = false
                    await handleItemFailure(item)
                    return
                }
                // Only the deadline expiring is a verdict about the media. A
                // cancelled task says nothing about the picture.
                if let preparation = error as? PlaybackPreparationError,
                   case .timedOut = preparation,
                   await retryAfterReadinessTimeout(at: startMs) { return }
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
        playbackRecoveryMonitor.reset()
        deliveryStarvation.reset()
        failed = false
        isChangingStream = false
        updateNowPlaying()
        // The status observer holds its fire while `isChangingStream` is up,
        // so a replacement that failed during its own attach is caught here.
        if item.status == .failed {
            await handleItemFailure(item)
            return
        }
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
        diagnosticSessionStatus = nil
        diagnosticSessionStatusObservedAt = nil
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
    nonisolated static func pictureInPictureUnavailableNotice(
        pgsOverlayIsActive: Bool
    ) -> String? {
        pgsOverlayIsActive ? pgsOverlayExternalPlaybackNotice : nil
    }

    func allowsPictureInPictureCommand() -> Bool {
        guard let notice = Self.pictureInPictureUnavailableNotice(
            pgsOverlayIsActive: pgsOverlayIsActive
        ) else {
            return true
        }
        showPlaybackNotice(notice)
        return false
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
        guard let polledSessionId = sessionId else { return }
        let generation = openGeneration
        statusTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, let model = self.model else { return }
                let status = try? await model.hlsStatus(polledSessionId)
                guard !Task.isCancelled,
                      self.started,
                      self.openGeneration == generation,
                      self.sessionId == polledSessionId
                else { return }
                self.sessionStatus = status
                if let status {
                    self.diagnosticSessionStatus = status
                    self.diagnosticSessionStatusObservedAt = Date()
                    // A fired wedge recovery replaces the session — and
                    // `open()` cancels THIS poll task, so the recovery must
                    // not run inside it. `observeDeliveryStarvation` spawns
                    // an unstructured task (immune to this task's
                    // cancellation) and this loop ends; the successor
                    // session starts its own poll.
                    if self.observeDeliveryStarvation(status) { return }
                }
                try? await Task.sleep(nanoseconds: 2_000_000_000)
            }
        }
    }

    /// Server-truth wedge check, run on every 2-second status poll. AVPlayer
    /// can stop fetching published media while reporting any
    /// `timeControlStatus` it likes and while its film clock still ticks —
    /// states the position-clock monitor reads as healthy or keeps resetting
    /// on. The server's delivery clock cannot be fooled that way: when it says
    /// nothing was delivered for 16+ seconds while 10+ seconds of published
    /// media sit unfetched, and this controller still wants playback with no
    /// change or seek in flight, that is a stall whatever AVPlayer claims.
    /// Recovery goes through the same bounded same-delivery ladder as every
    /// other stall — one reopen, then the visible failure screen. Returns
    /// whether a recovery was fired, so the polling task that observed it
    /// can end itself: the recovery runs in an unstructured task because
    /// `reopen` → `open()` cancels the status-poll task, and a recovery
    /// awaited inline there would cancel itself mid-open.
    private func observeDeliveryStarvation(_ status: PlaybackSessionStatus) -> Bool {
        let eligible = started
            && wantsPlayback
            && !finished
            && !failed
            && !isChangingStream
            && seekState.pendingMs == nil
            && player.currentItem != nil
        guard deliveryStarvation.observe(
            deliveredIdleMs: status.deliveredIdleMs,
            publishedEndMs: status.publishedEndMs,
            fetchedEndMs: status.fetchedEndMs,
            eligible: eligible
        ) else { return false }
        let position = realPositionMs()
        currentMs = position
        let event = PlaybackStallEvent(
            kind: .delivery,
            action: .reopen,
            positionMs: position,
            durationMs: status.deliveredIdleMs ?? 0
        )
        Task { await self.retrySameDeliveryAfterStall(event) }
        return true
    }

    /// Sample the film clock independently of AVPlayer's periodic observer,
    /// which stops firing when that clock stops. One shared clock counts the
    /// stagnation whatever `timeControlStatus` reports — a starving session
    /// flaps between `.playing` and `.waitingToPlayAtSpecifiedRate` faster
    /// than any per-regime counter's threshold, which is how real freezes
    /// used to go undetected. The regime tally labels the fired event
    /// instead: mostly-waiting stagnation is `.buffering` and reconnects the
    /// exact delivery; only a mostly-"playing" stagnation may consult the
    /// codec/HDR compatibility ladder, so the false SDR fallbacks that
    /// originally caused buffering to be excluded stay dead. This restores
    /// the in-player equivalent of closing and reopening a title.
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
                    // filling its publish gate, so an unestablished item gets
                    // a longer leash (~30 s) and no nudge before its bounded
                    // reopen — long enough not to duplicate a cold start,
                    // finite so a reopen that lands into continued starvation
                    // can no longer freeze forever with no error.
                    establishedPlayback: self.attachmentRecovery.establishedPlayback
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
                    // Only a mostly-"playing" stagnation may consult the HDR
                    // ladder; every other kind is transport recovery.
                    if stallEvent.kind != .silent {
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
        var decision = sameDeliveryStallRecovery.next(for: event.kind)
        // The per-epoch budget above resets on five seconds of real progress,
        // so a session that keeps almost-recovering can spend it repeatedly.
        // The rolling reopen budget is the backstop that turns that loop into
        // the visible failure screen.
        if case .reopen = decision, !recoveryReopenBudget.admit() {
            decision = .stop(event.kind.terminalState)
        }
        reportPlaybackStall(event, outcome: decision.outcome)
        switch decision {
        case .reopen:
            ttffReason = "stall-\(event.kind.rawValue)"
            ttffMeasurement.opened(at: event.positionMs)
            #if os(iOS)
            if Self.recoveryTransport(hasOfflineAsset: offlineAssetURL != nil) == .offlineAsset {
                await reloadOffline(at: event.positionMs)
                return
            }
            #endif
            await reopen(at: event.positionMs)
        case .stop(let terminal):
            player.pause()
            isPlaying = terminal.isPlaying
            wantsPlayback = terminal.wantsPlayback
            isChangingStream = false
            failed = terminal.failed
            playbackFailureTitle = Self.playbackStoppedFailureTitle
            playbackError = terminal.message
        }
    }

    nonisolated static func recoveryTransport(
        hasOfflineAsset: Bool
    ) -> SameDeliveryRecoveryTransport {
        hasOfflineAsset ? .offlineAsset : .serverSession
    }

    private func fail(_ error: Error) {
        isChangingStream = false
        ttffMeasurement.reset()
        failed = player.currentItem == nil || error is PlaybackPreparationError
        playbackFailureTitle = currentMs > 0
            ? Self.playbackStoppedFailureTitle
            : Self.playbackStartFailureTitle
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
                if let overlayPosition = PGSOverlayPolicy.periodicRefreshPosition(
                    currentMs: self.currentMs,
                    overlayIsActive: self.pgsOverlayIsActive
                ) {
                    self.refreshPGSOverlayWindow(at: overlayPosition)
                }
                // The last rate the viewer was genuinely playing at, so a
                // pause at 1.5× is restored as 1.5× and not as the 0 the
                // transport reports while paused (P2-5).
                let observedPosition = self.realPositionMs()
                let isActuallyPlaying = self.player.timeControlStatus == .playing
                    && self.player.rate > 0
                self.reportPlaybackTTFFIfNeeded(
                    at: observedPosition,
                    playing: isActuallyPlaying
                )
                // Sampled outside the playing branch so a picture that arrives
                // while paused still retires the watchdog.
                if self.blackFrameWatchdog.observe(
                    positionMs: observedPosition,
                    presentationSize: self.presentationSize,
                    hasVideoSource: self.decision?.source?.videoCodec != nil,
                    playing: isActuallyPlaying
                ) {
                    Task { @MainActor [weak self] in
                        guard let self else { return }
                        await self.handleBlackFrameDecodeFailure(at: observedPosition)
                    }
                }
                if isActuallyPlaying {
                    self.preferredRate = self.player.rate
                    self.attachmentRecovery.observe(
                        positionMs: observedPosition,
                        playing: true
                    )
                    if self.attachmentRecovery.establishedPlayback {
                        self.sameDeliveryStallRecovery.reset()
                        // The most recent same-delivery recovery proved itself.
                        // A later, independent interruption may reconnect once
                        // too; an immediate repeated failure remains terminal.
                        self.establishedHDRRetryAttempted = false
                    }
                }
                self.stallObservation.noteRecoveredStagnation(
                    self.playbackRecoveryMonitor.takeRecoveredStagnantDurationMs()
                )
                self.observeProgressPastEarlyEnd(self.currentMs)
                self.updateNowPlaying()
                if self.isPlaying && self.currentMs - self.lastReportedMs >= 10_000 {
                    self.lastReportedMs = self.currentMs
                    self.report(self.currentMs)
                    self.reportObservedPlaybackStalls(at: self.currentMs)
                }
            }
        }
    }

    /// Record the bounded retry before it happens, and clear it on every
    /// terminal route. Keeping this state transition outside the notification
    /// closure makes the actual controller wiring regression-testable.
    func prepareObservedEndAction(
        knownDurationMs: Int,
        itemDurationMs: Int?,
        isGrowingPlaylist: Bool,
        endedAt: Int
    ) -> PlayerItemEndAction {
        let action = Self.endAction(
            knownDurationMs: knownDurationMs,
            itemDurationMs: itemDurationMs,
            isGrowingPlaylist: isGrowingPlaylist,
            endedAt: endedAt,
            previousUncorroboratedEndMs: lastUncorroboratedEndMs
        )
        switch action {
        case .reopen:
            lastUncorroboratedEndMs = endedAt
        case .stop, .finish:
            lastUncorroboratedEndMs = nil
        }
        return action
    }

    /// Advancing one tolerance window past the failed boundary proves the
    /// replacement made progress and earns a fresh bounded retry later.
    func observeProgressPastEarlyEnd(_ positionMs: Int) {
        guard let endedAt = lastUncorroboratedEndMs,
              positionMs >= endedAt,
              positionMs - endedAt >= Self.repeatedEndToleranceMs
        else { return }
        lastUncorroboratedEndMs = nil
    }

    func stopAfterRepeatedEarlyEnd(
        at endedAt: Int,
        expectedDurationMs: Int?,
        isGrowingPlaylist: Bool
    ) {
        lastUncorroboratedEndMs = nil
        player.pause()
        currentMs = max(0, endedAt)
        report(currentMs)
        isPlaying = false
        wantsPlayback = false
        failed = true
        finished = false
        playbackFailureTitle = Self.earlyEndFailureTitle
        playbackError = Self.repeatedEarlyEndMessage
        reportEarlyEndFailure(
            at: currentMs,
            expectedDurationMs: expectedDurationMs,
            isGrowingPlaylist: isGrowingPlaylist
        )
        updateNowPlaying()
    }

    func finishAfterObservedEnd(at durationMs: Int) {
        lastUncorroboratedEndMs = nil
        isPlaying = false
        wantsPlayback = false
        currentMs = max(0, durationMs)
        report(currentMs)
        updateNowPlaying()
        finished = true
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
                let itemDurationSeconds = item.duration.seconds
                let itemDurationMs = itemDurationSeconds.isFinite && itemDurationSeconds > 0
                    ? Int(itemDurationSeconds * 1000)
                    : nil
                let isGrowingPlaylist = self.sessionId != nil && !self.isVOD
                switch self.prepareObservedEndAction(
                    knownDurationMs: self.knownDurationMs,
                    itemDurationMs: itemDurationMs,
                    isGrowingPlaylist: isGrowingPlaylist,
                    endedAt: endedAt
                ) {
                case .reopen:
                    // The viewer did not pause — the playlist merely announced
                    // its current end — and `wantsPlayback` still says so, so
                    // this continuation keeps playing. The repeated-position
                    // guard alone cannot stop a loop whose reopens land a few
                    // seconds apart each time (observed: nine sessions in
                    // sixteen seconds); the shared rolling budget can.
                    guard self.recoveryReopenBudget.admit() else {
                        let expectedDurationMs = self.knownDurationMs > 0
                            ? self.knownDurationMs
                            : itemDurationMs
                        self.stopAfterRepeatedEarlyEnd(
                            at: endedAt,
                            expectedDurationMs: expectedDurationMs,
                            isGrowingPlaylist: isGrowingPlaylist
                        )
                        return
                    }
                    await self.reopen(at: endedAt)
                    return
                case .stop:
                    let expectedDurationMs = self.knownDurationMs > 0
                        ? self.knownDurationMs
                        : itemDurationMs
                    self.stopAfterRepeatedEarlyEnd(
                        at: endedAt,
                        expectedDurationMs: expectedDurationMs,
                        isGrowingPlaylist: isGrowingPlaylist
                    )
                    return
                case .finish(let durationMs):
                    self.finishAfterObservedEnd(at: durationMs)
                }
            }
        }
    }

    private func observeStatus(of item: AVPlayerItem) {
        itemStatusObservation = item.observe(\.status, options: [.initial, .new]) { [weak self] item, _ in
            guard item.status == .failed else { return }
            Task { @MainActor in
                await self?.handleItemFailure(item)
            }
        }
    }

    /// A failed item advances the recovery ladder — unless a stream change is
    /// already replacing it. The predecessor's failure during a seek or track
    /// change is expected noise: server-side supersession deletes its playlist
    /// the moment the successor's create begins, so its outstanding fetches
    /// 404 while the new session spins up. Reacting to that raced a second
    /// open against the one in flight — flipping delivery down the SDR or
    /// transcode ladder, or stopping playback outright, mid-seek. `open()`
    /// re-checks the successor's own status once the change lands, so a
    /// genuinely failed replacement is still handled.
    private func handleItemFailure(_ item: AVPlayerItem) async {
        guard player.currentItem === item, !isChangingStream else { return }
        let event = item.errorLog()?.events.last
        let isCompatibilityFailure = Self.isCompatibilityPlaybackFailure(
            error: item.error as NSError?,
            eventDomain: event?.errorDomain,
            eventStatus: event?.errorStatusCode,
            eventComment: event?.errorComment
        )
        // Evaluated before anything recovers, because the log has to go out
        // before the ladder's own reopen replaces this item. An established
        // HDR delivery reconnects itself instead of descending, so it buys no
        // rung — say so rather than naming one that never ran.
        let reconnectsEstablishedHDR = Self.shouldPreserveEstablishedHDRDelivery(
            deliveredRange: deliveredRange,
            establishedPlayback: attachmentRecovery.establishedPlayback
        )
        let fallback: PlaybackCompatibilityFallback =
            started && isCompatibilityFailure && !reconnectsEstablishedHDR
                ? plannedCompatibilityFallback
                : PlaybackCompatibilityFallback.none
        reportPlaybackFailure(
            item,
            step: PlaybackCompatibilityLadderStep(cause: .itemFailure, fallback: fallback)
        )
        if started {
            // P2-6: this item is already dead, so its `currentTime()`
            // is 0 or invalid and a VOD/direct retry would silently
            // restart the film at 0:00. The last position the periodic
            // observer saw is the truthful retry point. The transport
            // intent survives in `wantsPlayback`, so a viewer who was
            // paused when the item failed stays paused.
            let position = Self.compatibilityRetryPositionMs(lastObservedMs: currentMs)
            if await retryEstablishedHDRDelivery(at: position) { return }
            if isCompatibilityFailure,
               await retryWithNextCompatibilityFallback(at: position) { return }
        }
        player.pause()
        isPlaying = false
        wantsPlayback = false
        isChangingStream = false
        failed = true
        playbackFailureTitle = currentMs > 0
            ? Self.playbackStoppedFailureTitle
            : Self.playbackStartFailureTitle
        playbackError = item.error?.localizedDescription
            ?? PlaybackPreparationError.failed.localizedDescription
    }

    private func reportPlaybackFailure(
        _ item: AVPlayerItem,
        step: PlaybackCompatibilityLadderStep
    ) {
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
                eventComment: event?.errorComment,
                ladderStep: step
            )
        )
        postClientLog(payload)
    }

    /// A ladder entry the client decided on its own: no AVFoundation error
    /// exists to describe it, so the log carries only the cause and the rung.
    private func reportCompatibilityLadderFailure(
        message: String,
        step: PlaybackCompatibilityLadderStep
    ) {
        #if os(iOS)
        if offlineId != nil { return }
        #endif
        postClientLog(ApplePlaybackFailureLog(
            message: message,
            method: clientLogMethod,
            code: nil,
            title: title,
            fileId: fileId,
            vcodec: decision?.source?.videoCodec,
            detail: Self.playbackFailureDetail(
                error: nil,
                eventDomain: nil,
                eventStatus: nil,
                eventComment: nil,
                ladderStep: step
            )
        ))
    }

    private func reportEarlyEndFailure(
        at positionMs: Int,
        expectedDurationMs: Int?,
        isGrowingPlaylist: Bool
    ) {
        #if os(iOS)
        if offlineId != nil { return }
        #endif
        guard model != nil else { return }
        postClientLog(ApplePlaybackEarlyEndLog(
            positionMs: positionMs,
            expectedDurationMs: expectedDurationMs,
            isGrowingPlaylist: isGrowingPlaylist,
            message: Self.repeatedEarlyEndMessage,
            method: clientLogMethod,
            title: title,
            fileId: fileId,
            vcodec: decision?.source?.videoCodec
        ))
    }

    private func reportPlaybackTTFFIfNeeded(at positionMs: Int, playing: Bool) {
        #if os(iOS)
        if offlineId != nil { return }
        #endif
        guard let ms = ttffMeasurement.observe(positionMs: positionMs, playing: playing) else {
            return
        }
        let method = clientLogMethod
        let height = sessionStatus?.targetHeight
            ?? selectedHeight
            ?? (method == "transcode" ? nil : decision?.source?.height)
        postClientLog(ApplePlaybackTTFFLog(
            ms: ms,
            method: method,
            title: title,
            fileId: fileId,
            vcodec: decision?.source?.videoCodec,
            height: height,
            encoder: encoder,
            sessionId: sessionId,
            attempt: playbackAttemptId,
            reason: ttffReason
        ))
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
            encoder: encoder,
            sessionId: sessionId,
            attempt: playbackAttemptId,
            snapshot: playbackDiagnosticSnapshot(at: event.positionMs)
        )
        postClientLog(payload)
    }

    private func reportObservedPlaybackStalls(at positionMs: Int) {
        #if os(iOS)
        if offlineId != nil { return }
        #endif
        guard let observed = stallObservation.take(numberOfStalls: stalls) else { return }
        postClientLog(ApplePlaybackObservedStallLog(
            delta: observed.delta,
            positionMs: positionMs,
            stagnantDurationMs: observed.stagnantDurationMs,
            method: clientLogMethod,
            title: title,
            fileId: fileId,
            vcodec: decision?.source?.videoCodec,
            encoder: encoder,
            sessionId: sessionId,
            attempt: playbackAttemptId,
            snapshot: playbackDiagnosticSnapshot(at: positionMs)
        ))
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

    /// `ladderStep` names the recovery this failure bought, so a device log
    /// shows which rung was chosen and what chose it — including for the two
    /// client-observed verdicts (a readiness timeout, a black picture) that
    /// carry no AVFoundation error at all.
    static func playbackFailureDetail(
        error: NSError?,
        eventDomain: String?,
        eventStatus: Int?,
        eventComment: String?,
        ladderStep: PlaybackCompatibilityLadderStep? = nil
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
        if let ladderStep {
            fields.append("cause=\(ladderStep.cause.rawValue)")
            fields.append("ladder=\(ladderStep.fallback.telemetryName)")
        }
        return fields.joined(separator: " · ")
    }

    static func shouldRetryWithCompatibilityTranscode(
        canRetry: Bool,
        alreadyAttempted: Bool
    ) -> Bool {
        canRetry && !alreadyAttempted
    }

    /// Route an interactive seek. A growing HLS session's playlist advertises
    /// a real seekable window — everything published and not yet pruned — and
    /// AVPlayer seeks inside it instantly. Replacing the server session is
    /// only necessary when the target lies outside that window (not yet
    /// transcoded, or already pruned by retention). Before this routing every
    /// non-VOD seek was a full session teardown and create: multi-second
    /// spinner per scrub, one server round trip per ±10 s press.
    ///
    /// `seekableRangesMs` is in the item's local clock; `targetMs`/`baseMs`
    /// are film time, with `baseMs` the film position of item-local zero.
    /// The holdback keeps a native landing short of the live edge, where no
    /// future media exists yet; a target just past the edge snaps to the
    /// holdback instead of paying a whole reopen for a couple of seconds.
    nonisolated static func seekRoute(
        targetMs: Int,
        baseMs: Int,
        usesDirectTimeline: Bool,
        isVOD: Bool,
        isChangingStream: Bool,
        seekableRangesMs: [ClosedRange<Int>],
        liveEdgeHoldbackMs: Int = 1_500,
        liveEdgeSnapWindowMs: Int = 2_500
    ) -> PlayerSeekRoute {
        // A change in flight owns the player; the reopen queue serializes
        // behind it. This branch also keeps the old invariant that the
        // native leg below never runs with a nonzero base on VOD/direct.
        if isChangingStream { return .reopen }
        if usesDirectTimeline || isVOD {
            // Fully seekable timelines whose local clock is film time.
            return .native(itemMs: targetMs)
        }
        let local = targetMs - baseMs
        // Exact containment first, across every advertised range: a target
        // inside a later range must seek there, not snap to an earlier
        // range's edge (PR #122 review: [0…10 s, 11 s…90 s] with a 12 s
        // target belongs at 12 s, not at the first range's 8.5 s holdback).
        for range in seekableRangesMs {
            let safeUpper = range.upperBound - liveEdgeHoldbackMs
            guard safeUpper >= range.lowerBound else { continue }
            if local >= range.lowerBound && local <= safeUpper {
                return .native(itemMs: local)
            }
        }
        // Snapping applies only at the live edge — the range with the
        // greatest upper bound — never across an interior gap, whose media
        // genuinely is not in the playlist.
        if let latest = seekableRangesMs.max(by: { $0.upperBound < $1.upperBound }) {
            let safeUpper = latest.upperBound - liveEdgeHoldbackMs
            if safeUpper >= latest.lowerBound,
               local > safeUpper,
               local <= latest.upperBound + liveEdgeSnapWindowMs {
                return .native(itemMs: safeUpper)
            }
        }
        return .reopen
    }

    /// Whether this sample was taken during an explicit network wait. The
    /// shared stall clock counts regardless — crossing regimes must never
    /// restart it — but the tally of waiting samples decides the fired
    /// event's kind, and `.buffering` never enters the codec/HDR ladder.
    /// This applies equally to iPhone, iPad, and Apple TV.
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
        // CoreMedia/OSStatus error instead of an AVError.Code. The two
        // CoreMedia codes below are media verdicts too, not transport faults:
        // a device with no Dolby Vision Profile 5 decoder rejects the asset
        // outright (-12927), or fails to build a decoder session from the
        // empty `hvcC` such a track carries (-15517). Leaving them out
        // classified the most common Dolby Vision failure as terminal, so the
        // ladder never ran and the viewer kept a black screen.
        let decoderStatusCodes: Set<Int> = [
            -12906, // kVTCouldNotFindVideoDecoderErr
            -12909, // kVTVideoDecoderBadDataErr
            -12910, // kVTVideoDecoderUnsupportedDataFormatErr
            -12911, // kVTVideoDecoderMalfunctionErr
            -12927, // kFigPlayerError_IncompatibleAsset
            -15517, // decoder initialization failed (empty hvcC)
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
    nonisolated static let pgsOverlayExternalPlaybackNotice =
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

    /// Whether the subtitle a starting playback wants must be dropped because
    /// drawing it would have traded the HDR picture away.
    ///
    /// Two vetoes, and the server's counts **only when a subtitle choice
    /// actually traveled**. `/decision` emits its `selection` block whenever
    /// *either* parameter was sent (`selection_requested` is
    /// `audio.is_some() || subtitle.is_some()`, crates/plurxd/src/http/stream.rs),
    /// and the `subtitle_burn_in_blocked_by_hdr` it carries is computed from
    /// the *policy* subtitle against the *source* range. On an audio-only
    /// request that describes a burn the server never even considered applying
    /// — `apply_selected_subtitle` is gated on `subtitle=`. Reading the flag
    /// there would silently drop a forced track the no-choice path starts
    /// happily, e.g. an HDR source tone-mapped to SDR for an SDR-only device
    /// whose policy pick is forced PGS signs: picking only a Japanese audio
    /// track would turn the signs off.
    ///
    /// The client's own predicate reads the *delivered* range, so it answers
    /// for whatever is actually about to play and applies to both arms.
    static func startingSubtitleBlockedByHDR(
        prePlaySubtitle: Int?,
        wanted: Int?,
        serverSelection: DecisionSelection?,
        tracks: [SubtitleTrack],
        deliveredRange: String?
    ) -> Bool {
        let serverRefused = prePlaySubtitle != nil
            && serverSelection?.subtitleBurnInBlockedByHdr == true
        return serverRefused
            || subtitleBurnWouldDiscardHDR(wanted, tracks: tracks, deliveredRange: deliveredRange)
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
            establishedPlayback: attachmentRecovery.establishedPlayback
        ) else { return false }
        guard !establishedHDRRetryAttempted else {
            player.pause()
            isPlaying = false
            wantsPlayback = false
            isChangingStream = false
            failed = true
            playbackFailureTitle = Self.playbackStoppedFailureTitle
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

    /// The rung the ladder would choose right now. Read before the ladder runs
    /// so the failure log can name the step the viewer actually got, and so a
    /// client-observed verdict with no rung left changes nothing at all.
    private var plannedCompatibilityFallback: PlaybackCompatibilityFallback {
        Self.nextCompatibilityFallback(
            canRetryWithHDRBase: canRetryCurrentItemWithHDRBase,
            hdrBaseAlreadyAttempted: dolbyVisionFallbackAttempted,
            canRetryWithTranscode: canRetryCurrentItemWithTranscode,
            transcodeAlreadyAttempted: compatibilityFallbackAttempted
        )
    }

    /// A fresh start that never reached `.readyToPlay` is a media verdict, not
    /// a transport fault: AVFoundation neither failed the item nor readied it,
    /// which is what a Dolby Vision Profile 5 track with an empty `hvcC` does
    /// on a device that cannot decode it. Spend one rung instead of stopping.
    /// Returning false leaves the caller's original timeout to surface.
    private func retryAfterReadinessTimeout(at position: Int) async -> Bool {
        let fallback = plannedCompatibilityFallback
        guard fallback != .none else { return false }
        reportCompatibilityLadderFailure(
            message: Self.readinessTimeoutMessage,
            step: PlaybackCompatibilityLadderStep(
                cause: .readinessTimeout,
                fallback: fallback
            )
        )
        return await retryWithNextCompatibilityFallback(at: position)
    }

    /// Six seconds of film clock with nothing decoded. Nothing was ever
    /// established here — no frame has been presented for this item — so the
    /// established-HDR policy does not apply and the pre-start ladder does.
    /// With no rung left this deliberately does nothing: audio is still
    /// playing, and there is no better delivery left to try.
    private func handleBlackFrameDecodeFailure(at position: Int) async {
        guard started, !isChangingStream, player.currentItem != nil else { return }
        let fallback = plannedCompatibilityFallback
        guard fallback != .none else { return }
        reportCompatibilityLadderFailure(
            message: Self.blackFrameFailureMessage,
            step: PlaybackCompatibilityLadderStep(
                cause: .blackFrames,
                fallback: fallback
            )
        )
        _ = await retryWithNextCompatibilityFallback(
            at: Self.compatibilityRetryPositionMs(lastObservedMs: position)
        )
    }

    private func report(_ position: Int) {
        guard position > 0 else { return }
        let globalPosition = AudiobookTimeline.globalPosition(
            localPositionMs: position,
            partOffsetMs: progressOffsetMs
        )
        let duration = itemDurationMs ?? (knownDurationMs > 0 ? knownDurationMs : nil)
        #if os(iOS)
        if let offlineId {
            Task {
                await OfflineDownloadManager.shared.recordProgress(
                    id: offlineId,
                    positionMs: globalPosition,
                    durationMs: duration
                )
            }
            return
        }
        #endif
        let itemId = itemId
        let model = model
        Task { await model?.reportProgress(itemId: itemId, positionMs: globalPosition, durationMs: duration) }
    }

    /// Wait for the attached item to reach `.readyToPlay`, or give up.
    ///
    /// AVPlayerItem's KVO publisher is not guaranteed to deliver another
    /// value when a tvOS network request stalls. The old unbounded
    /// `for await` consequently held the initial `play()` forever and left
    /// the transport looking paused. Poll the authoritative status with a
    /// finite deadline so playback either resumes or surfaces a useful
    /// connection error.
    private func awaitItemReady(_ item: AVPlayerItem) async throws {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: .seconds(Self.itemReadinessDeadlineSeconds))
        while item.status == .unknown {
            try Task.checkCancellation()
            guard clock.now < deadline else { throw PlaybackPreparationError.timedOut }
            try await Task.sleep(for: .milliseconds(100))
        }

        if item.status == .failed {
            throw item.error ?? PlaybackPreparationError.failed
        }
        guard item.status == .readyToPlay else { throw PlaybackPreparationError.failed }
    }

    private func seekWhenReady(_ item: AVPlayerItem, ms: Int) async throws {
        try await awaitItemReady(item)

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
        if let mediaOriginMs = hls.mediaOriginMs { return max(0, mediaOriginMs) }
        return Int((hls.startSeconds ?? Double(requestedStartMs) / 1000.0) * 1000)
    }

    /// Item-local seek needed after a live copy session opens on a keyframe
    /// before the requested film position. Tiny timestamp rounding differences
    /// stay at zero; only a real keyframe lead-in pays for an exact seek.
    nonisolated static func sessionAttachSeekMs(
        requestedStartMs: Int,
        mediaOriginMs: Int,
        minimumCorrectionMs: Int = 100
    ) -> Int? {
        let correction = max(0, requestedStartMs) - max(0, mediaOriginMs)
        return correction >= minimumCorrectionMs ? correction : nil
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

    /// The audio index an HLS session must carry, in falling order of
    /// authority: `explicit` is a later in-player choice, `plan` is what the
    /// server put in the delivery plan for a selection-aware `/decision`, and
    /// `selected` is the server's automatic answer.
    ///
    /// `plan` outranks `selected` because the contract says to execute the plan
    /// as given (docs/CLIENTS.md §1); it sits *below* `explicit` because the
    /// plan was decided before the viewer changed their mind mid-playback, and
    /// a reopen replays that stale plan otherwise.
    nonisolated static func sessionAudioIndex(
        explicit: Int?,
        plan: Int?,
        selected: Int?
    ) -> Int? {
        explicit ?? plan ?? selected
    }

    /// The subtitle a starting playback shows.
    ///
    /// A pre-play choice is the viewer speaking, so it outranks both this
    /// device's "subtitles off" setting and the never-start-a-burn veto that
    /// governs *automatic* selection — picking a bitmap track on the detail
    /// screen is exactly the viewer asking for it. Off (`-1`) is a choice too,
    /// and the server echoes it as no effective subtitle.
    ///
    /// With no pre-play choice this is unchanged: the device's Off setting is
    /// honored first, then the server's own pick via `automaticSubtitleIndex`.
    static func initialSubtitleIndex(
        prePlay: Int?,
        serverSelection: DecisionSelection?,
        tracks: [SubtitleTrack],
        deviceSubtitlesOff: Bool
    ) -> Int? {
        if let prePlay {
            // The echo is authoritative when there is one — `subtitle_index`
            // null then means Off. A server predating the selection contract
            // sends no `selection` block at all, so fall back to what was asked
            // for, minus the Off sentinel.
            guard let serverSelection else {
                return prePlay == PrePlaySelection.subtitleOff ? nil : prePlay
            }
            return serverSelection.subtitleIndex
        }
        // "Off" in this device's settings is the viewer saying never, so it is
        // honored before the server's pick is even looked at. It is the
        // viewer's instruction rather than a second copy of the selection rule,
        // which lives on the server alone.
        return deviceSubtitlesOff ? nil : automaticSubtitleIndex(tracks)
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
    nonisolated static func titleMarksForced(_ title: String) -> Bool {
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
    nonisolated private static func negatedBefore(_ prefix: String) -> Bool {
        let word = prefix
            .split(whereSeparator: { !isAlphanumeric($0) })
            .last
            .map(String.init) ?? ""
        return ["non", "not", "no", "never"].contains(word)
    }

    nonisolated private static func isAlphanumeric(_ character: Character) -> Bool {
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
