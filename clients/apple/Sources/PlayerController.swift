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
            return "The video took too long to prepare. Check that the Plurx server is reachable."
        case .failed:
            return "The video stream could not be prepared. Check the server and try again."
        }
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

/// Drives one AVPlayer and executes the server-owned delivery plan. It also
/// supplies the controls AVPlayer withholds for a growing EVENT playlist: an
/// explicit on-demand timeline, reliable play/pause commands, server playback
/// telemetry, and stream restarts for audio, quality, and burn-only subtitle
/// changes. Ordinary text subtitles switch through AVPlayer media selection —
/// once the stream carries their renditions, which under
/// `SubtitleReadiness.onDemand` is from the first selection rather than from
/// the first frame (`needsNativeSubtitleSession`).
@MainActor
final class PlayerController: ObservableObject {
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
    @Published private(set) var finished = false

    private var baseMs = 0
    private var itemId = 0
    private var fileId = 0
    private var title = ""
    private var audioOverride: Int?
    private weak var model: AppModel?
    private var timeObserver: Any?
    private var endObserver: NSObjectProtocol?
    private var itemStatusObservation: NSKeyValueObservation?
    private var statusTask: Task<Void, Never>?
    private var started = false
    private var sessionId: String?
    private var lastReportedMs = 0
    private var usesDirectTimeline = false
    private var canRetryCurrentItemWithTranscode = false
    private var compatibilityFallbackAttempted = false
    private var forceCompatibilityTranscode = false
    /// A burn is part of the current video frames. Leaving one requires one
    /// reopen; native-to-native and native-to-Off never do.
    private var activeBurnedSubtitle: Int?
    /// Read once, in `start`, rather than per open: a viewer who changes the
    /// setting from another device or another tab of Settings must not have the
    /// stream rebuilt under them mid-film. The choice a title started with is
    /// the choice it finishes with.
    private var subtitleReadiness: SubtitleReadiness = .instant
    /// Sticky for this playback. Once a native text track has been asked for —
    /// by automatic selection at cold start, or by the viewer — the stream keeps
    /// its subtitle renditions, including after subtitles are turned off again:
    /// dropping back to direct play would be a second restart nobody asked for,
    /// and the next selection would only have to pay for a third.
    private var wantsNativeSubtitleRenditions = false
    /// True while the attached item is the raw file URL. It carries no subtitle
    /// renditions, so the first native selection against it has to rebuild the
    /// stream instead of switching AVPlayer's media selection in place.
    private var isDirectPlayback = false
    /// Holds the newest seek/track intent that arrived mid-change so it wins
    /// instead of vanishing.
    private var reopenQueue = PlayerReopenQueue()
    /// Stable for this player instance. Server-side supersession uses it to
    /// replace this player's own stream without touching another device.
    private let playbackId = UUID().uuidString

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
        if activeBurnedSubtitle != nil { return "Transcode · subtitle burn-in" }
        if selectedHeight != nil { return "Transcode · \(selectedHeight!)p" }
        let mode = decision?.delivery?.mode ?? Self.legacyMode(decision?.method ?? "")
        switch mode {
        case "direct": return "Direct play"
        case "remux": return "Remux · HLS"
        default: return isVOD ? "Transcode · cached" : "Transcode"
        }
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
        self.title = title
        subtitleReadiness = model.subtitleReadiness
        wantsNativeSubtitleRenditions = false
        isDirectPlayback = false

        #if os(iOS)
        // iOS needs an explicit playback audio session for silent-switch and
        // background/PiP behavior. Activating it on tvOS was a regression:
        // Apple TV owns the output route and AVPlayer can remain waiting even
        // though the item and server are healthy.
        try? AVAudioSession.sharedInstance().setCategory(.playback)
        try? AVAudioSession.sharedInstance().setActive(true)
        installRemoteCommands()
        #endif

        applyLanguagePrefs(audio: model.audioLang, sub: model.subLang)
        player.appliesMediaSelectionCriteriaAutomatically = true
        player.automaticallyWaitsToMinimizeStalling = true
        addPeriodicObserver()

        Task { await load(startMs: startMs) }
    }

    func togglePlayPause() {
        if player.timeControlStatus == .playing || player.rate > 0 {
            player.pause()
            isPlaying = false
        } else {
            player.play()
            isPlaying = true
        }
        updateNowPlaying()
    }

    func skip(seconds: Double) {
        seek(toMs: realPositionMs() + Int(seconds * 1000))
    }

    func skipActiveMarker() {
        guard let marker = activeMarker else { return }
        seek(toMs: marker.endMs)
    }

    func seek(toMs requested: Int) {
        let upper = knownDurationMs > 0 ? max(0, knownDurationMs - 2_000) : Int.max
        let target = min(max(0, requested), upper)
        Task {
            if usesDirectTimeline || isVOD {
                _ = await player.seek(
                    to: CMTime(seconds: Double(target) / 1000.0, preferredTimescale: 600),
                    toleranceBefore: .zero,
                    toleranceAfter: .zero
                )
                currentMs = target
                updateNowPlaying()
            } else {
                await reopen(at: target)
            }
        }
    }

    func selectSubtitle(_ index: Int?) {
        guard index != selectedSubtitle else { return }
        selectedSubtitle = index
        let requiresReopen = Self.subtitleSelectionRequiresReopen(
            index: index,
            tracks: subtitles,
            hasActiveBurn: activeBurnedSubtitle != nil,
            isDirectPlayback: isDirectPlayback
        )
        // Set before the reopen is scheduled: the open it leads to reads this
        // to decide it may no longer direct-play.
        if let index, !Self.subtitleRequiresBurn(index, in: subtitles) {
            wantsNativeSubtitleRenditions = true
        }
        if requiresReopen {
            Task { await reopen(at: realPositionMs()) }
        } else {
            // Selection belongs to AVPlayerItem, not the HLS session. This is
            // the no-restart path that preserves video copy, HDR, position,
            // and the viewer's selected quality.
            Task { await applyNativeSubtitleSelection(index, to: player.currentItem) }
        }
    }

    func selectAudio(_ index: Int) {
        guard index != selectedAudio else { return }
        selectedAudio = index
        audioOverride = index
        Task { await reopen(at: realPositionMs()) }
    }

    func selectQuality(_ height: Int?) {
        guard height != selectedHeight else { return }
        selectedHeight = height
        Task { await reopen(at: realPositionMs()) }
    }

    /// Report the final position and hand any encoder back immediately.
    func stop() {
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
            selectedAudio = decision.audio?.first(where: { $0.default })?.index
            // Container defaults describe the muxer's primary language, not
            // this viewer. Choose only within the preferred language and
            // prefer a forced/narrative track before a full subtitle track.
            selectedSubtitle = Self.automaticSubtitleIndex(
                decision.subtitles ?? [],
                preferredLanguage: model.subLang
            )
            // An automatically selected native track has to be visible from the
            // first frame, so it needs its rendition even under `.onDemand`. A
            // forced bitmap track does not: it forces a burn transcode anyway.
            if let index = selectedSubtitle,
               Self.nativeSubtitleOrdinal(index, in: decision.subtitles ?? []) != nil {
                wantsNativeSubtitleRenditions = true
            }
            try await openAndDrain(decision: decision, at: startMs)
        } catch {
            reopenQueue.clear()
            fail(error)
        }
    }

    private func reopen(at position: Int) async {
        // A growing EVENT playlist can momentarily announce its current end,
        // and several UI actions can also request a restart. Never overlap two
        // server-session replacements (see `PlayerReopenQueue`) — but never
        // discard one either: a request that lands mid-change is remembered and
        // replayed as the single trailing reopen.
        guard let decision, started else { return }
        guard let next = reopenQueue.request(position, changeInFlight: isChangingStream) else {
            return
        }
        do {
            try await openAndDrain(decision: decision, at: next)
        } catch {
            reopenQueue.clear()
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
            guard let trailing = reopenQueue.takePending() else { return }
            next = trailing
        }
    }

    private func open(decision: Decision, at startMs: Int) async throws {
        guard let model, started else { return }
        let wasPlaying = isPlaying
        isChangingStream = true
        failed = false
        playbackError = nil
        player.pause()
        // The session this open replaces. It is retired only once its successor
        // exists: releasing first meant a failed create left the viewer's item
        // pointing at a playlist this client had just deleted — buffered runway,
        // then a stall, with `fail()` deliberately quiet because an item still
        // existed. Its telemetry poll stops now; the session itself does not.
        let superseded = sessionId
        stopStatusPolling()

        let normalMode = decision.delivery?.mode ?? Self.legacyMode(decision.method)
        let burnSubtitle = selectedSubtitle.flatMap { index in
            Self.subtitleRequiresBurn(index, in: subtitles) ? index : nil
        }
        let nativeSubtitle = selectedSubtitle.flatMap { index in
            Self.nativeSubtitleOrdinal(index, in: subtitles) == nil ? nil : index
        }
        let needsSubtitleRenditions = Self.needsNativeSubtitleSession(
            hasNativeTextTrack: subtitles.contains(where: \.isNativeHLS),
            readiness: subtitleReadiness,
            subtitlesInUse: wantsNativeSubtitleRenditions
        )
        let forceTranscode = burnSubtitle != nil || selectedHeight != nil || forceCompatibilityTranscode
        let customAudio = audioOverride != nil
        canRetryCurrentItemWithTranscode = normalMode != "transcode" && !forceTranscode && !customAudio
        let direct = normalMode == "direct" && !forceTranscode && !customAudio
            && !needsSubtitleRenditions
        let url: URL?
        var seekAfterAttach: Int?

        if direct {
            activeBurnedSubtitle = nil
            baseMs = 0
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
            let chosenAudio = audioOverride
            let aac = copy ? needsAAC(audioIndex: chosenAudio, decision: decision)
                : nil
            // A subtitle burn on a file the device could otherwise take keeps
            // source resolution. A genuine transcode still lets server Auto
            // choose its rung unless the viewer selected one explicitly.
            let burnHeight = burnSubtitle != nil && normalMode != "transcode"
                ? decision.source?.height : nil
            let body = CreateSessionRequest(
                playbackId: playbackId,
                height: selectedHeight ?? burnHeight,
                start: Double(startMs) / 1000.0,
                audio: chosenAudio,
                subtitleBurn: burnSubtitle,
                nativeSubtitles: true,
                subtitle: nativeSubtitle,
                copy: copy ? true : nil,
                aac: copy ? aac : nil,
                preserveDolbyVision: copy ? (decision.preserveDolbyVision ?? false) : nil
            )
            let hls: HlsStart
            do {
                hls = try await model.createHlsSession(fileId: fileId, body: body)
            } catch {
                // Nothing came into existence to replace the current stream, so
                // nothing about it changes: same session, same player item,
                // telemetry running again, and playing if it was. The caller
                // surfaces a transient error instead of a coming stall.
                restoreAfterFailedChange(wasPlaying: wasPlaying, session: superseded)
                throw error
            }
            guard started else {
                await model.endHlsSession(hls.sessionId)
                isChangingStream = false
                return
            }
            sessionId = hls.sessionId
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
            if isVOD {
                baseMs = 0
                if startMs > 0 { seekAfterAttach = startMs }
            } else {
                baseMs = Int((hls.startSeconds ?? Double(startMs) / 1000.0) * 1000)
            }
            url = Session.shared.url(hls.playlistUrl)
            startStatusPolling()
        }

        guard let url else {
            restoreAfterFailedChange(wasPlaying: wasPlaying, session: superseded)
            throw APIError.badURL
        }
        // Committed to this stream shape. A raw-file item has no legible
        // renditions, so `selectSubtitle` has to rebuild rather than switch.
        isDirectPlayback = direct
        // The successor exists — only now does the predecessor go. Server-side
        // supersession has already retired it (the create carried the same
        // `playback_id`); this DELETE just hands the encoder slot back at once
        // rather than at the idle reaper's convenience.
        if direct { sessionId = nil }
        await release(session: superseded)
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
        #if os(iOS)
        // The tvOS 26 SDK exposes this setter, but some shipping Apple TV
        // runtimes do not implement it and abort on the Objective-C selector.
        // iOS uses it for system presentation metadata; tvOS uses our custom
        // player chrome and does not need item-level external metadata.
        item.externalMetadata = [titleMetadata(title)]
        #endif
        observeEnd(of: item)
        observeStatus(of: item)
        player.replaceCurrentItem(with: item)
        // Start loading/playing immediately. Previously a resume point gated
        // this call behind item readiness and could leave tvOS permanently
        // presenting a stopped transport.
        player.play()
        isPlaying = true
        if let seekAfterAttach { try await seekWhenReady(item, ms: seekAfterAttach) }
        await applyNativeSubtitleSelection(nativeSubtitle, to: item)
        player.play()
        currentMs = startMs
        failed = false
        isChangingStream = false
        updateNowPlaying()
    }

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
            isPlaying = true
        }
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
                self.currentMs = self.realPositionMs()
                self.isPlaying = self.player.timeControlStatus == .playing
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
                self.isPlaying = false
                let endedAt = self.realPositionMs()
                // A growing EVENT playlist can momentarily end before the
                // title does. Only hand autoplay a genuine film/episode end.
                if self.knownDurationMs > 0 && endedAt < self.knownDurationMs - 15_000 {
                    await self.reopen(at: endedAt)
                    return
                }
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
                if Self.shouldRetryWithCompatibilityTranscode(
                    canRetry: self.canRetryCurrentItemWithTranscode,
                    alreadyAttempted: self.compatibilityFallbackAttempted
                ), self.started {
                    self.compatibilityFallbackAttempted = true
                    self.forceCompatibilityTranscode = true
                    self.canRetryCurrentItemWithTranscode = false
                    self.isChangingStream = false
                    self.playbackError = "The original stream could not open. Retrying a compatible stream…"
                    await self.reopen(at: self.realPositionMs())
                    return
                }
                self.player.pause()
                self.isPlaying = false
                self.isChangingStream = false
                self.failed = true
                self.playbackError = item.error?.localizedDescription
                    ?? PlaybackPreparationError.failed.localizedDescription
            }
        }
    }

    static func shouldRetryWithCompatibilityTranscode(
        canRetry: Bool,
        alreadyAttempted: Bool
    ) -> Bool {
        canRetry && !alreadyAttempted
    }

    private func report(_ position: Int) {
        guard position > 0 else { return }
        let duration = knownDurationMs > 0 ? knownDurationMs : nil
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

    /// Whether this open has to go through an HLS session for no reason other
    /// than making the file's text subtitles selectable. The whole of the
    /// `SubtitleReadiness` setting is this function; nothing else branches on
    /// it.
    ///
    /// `.instant` answers yes for any file carrying a native text track, which
    /// is the v0.2 behaviour and the default: every track exists as a rendition
    /// before the menu is ever opened, so switching one is free. `.onDemand`
    /// answers yes only once a native track has actually been asked for, so a
    /// play that never touches subtitles costs the server nothing — and the
    /// first selection pays for exactly one clean restart, the same one a burn
    /// already performs, at the same film position.
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

    /// Whether a subtitle selection has to rebuild the stream rather than move
    /// AVPlayer's media selection on the item already playing.
    ///
    /// Three reasons, and only three: leaving a burn (it is in the video
    /// frames), entering one, and — new with `.onDemand` — the first native
    /// pick while direct-playing, because a raw file URL has no renditions to
    /// select. Turning subtitles off during direct play reopens nothing; there
    /// was never anything on.
    static func subtitleSelectionRequiresReopen(
        index: Int?,
        tracks: [SubtitleTrack],
        hasActiveBurn: Bool,
        isDirectPlayback: Bool
    ) -> Bool {
        if hasActiveBurn { return true }
        guard let index else { return false }
        if subtitleRequiresBurn(index, in: tracks) { return true }
        return isDirectPlayback
    }

    /// Position in the HLS master rendition order. The server advertises only
    /// native tracks (`is_native_text_subtitle`) and preserves source order, so
    /// this remains stable even when bitmap, `mov_text`, and styled tracks are
    /// interleaved in the menu — provided `isNativeHLS` names the same set the
    /// server does, which is why it prefers the server's own `native` flag.
    static func nativeSubtitleOrdinal(_ index: Int, in tracks: [SubtitleTrack]) -> Int? {
        tracks.filter(\.isNativeHLS).firstIndex(where: { $0.index == index })
    }

    /// Text that is not native — `mov_text`, styled ASS/SSA — burns like a
    /// bitmap track. Routing it to a rendition instead gets a 400 from the
    /// create, and it is absent from the master either way.
    static func subtitleRequiresBurn(_ index: Int, in tracks: [SubtitleTrack]) -> Bool {
        tracks.first(where: { $0.index == index }).map { !$0.isNativeHLS } ?? true
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

    private func applyNativeSubtitleSelection(_ index: Int?, to item: AVPlayerItem?) async {
        guard let item, player.currentItem === item else { return }
        guard let group = try? await item.asset.loadMediaSelectionGroup(for: .legible) else { return }
        _ = Self.applyNativeSubtitleSelection(index, tracks: subtitles) { ordinal in
            let option = ordinal.flatMap { group.options.indices.contains($0) ? group.options[$0] : nil }
            item.select(option, in: group)
        }
    }

    private func applyLanguagePrefs(audio: String, sub: String) {
        player.setMediaSelectionCriteria(
            AVPlayerMediaSelectionCriteria(
                preferredLanguages: bcp47(audio),
                preferredMediaCharacteristics: nil
            ),
            forMediaCharacteristic: .audible
        )
        player.setMediaSelectionCriteria(
            AVPlayerMediaSelectionCriteria(
                preferredLanguages: sub == "off" ? [] : bcp47(sub),
                preferredMediaCharacteristics: nil
            ),
            forMediaCharacteristic: .legible
        )
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
        let map = [
            "eng": "en", "jpn": "ja", "spa": "es", "fre": "fr",
            "ger": "de", "ita": "it", "por": "pt", "kor": "ko",
            "chi": "zh", "rus": "ru", "hin": "hi", "ara": "ar",
        ]
        if let two = map[code] { return [two, code] }
        return [code]
    }

    /// The whole automatic-subtitle policy, in one pure function.
    ///
    /// > Automatic selection must never start a burn — except a forced track,
    /// > which may, always at source height.
    ///
    /// | Track shape | Automatic behavior |
    /// |---|---|
    /// | Forced (disposition flag *or* "forced" in the title), any codec | apply — the one permitted auto-burn |
    /// | Default-flagged and native text (`isNativeHLS`) | apply through the free rendition path |
    /// | Default-flagged but bitmap, `mov_text`, or styled | never automatic; explicit selection only |
    /// | Merely the same language | never automatic |
    ///
    /// The native row deliberately reads `isNativeHLS` — the server's own
    /// `native` flag where it is sent — and not the broader `text`. A
    /// default-flagged `mov_text` track is text, is absent from the HLS master,
    /// and 400s on an explicit pick; auto-applying it would caption nothing.
    ///
    /// The forced carve-out is the `forcedIndex` line and nothing else: delete
    /// it and automatic selection can no longer reach an encoder at all. It
    /// exists because a forced track marks dialogue the film is unintelligible
    /// without, so refusing it by default trades a comprehension failure for an
    /// encoder slot. A burn that does start here carries source height already
    /// (see `burnHeight` in `open`).
    ///
    /// Language is filtered first in every row: never fall back to a flagged
    /// container default in another language, because an Italian-first mux can
    /// otherwise caption an English-audio film in Italian. Some release muxes
    /// omit the forced disposition and retain only a "Forced" title, so both
    /// signals are meaningful.
    ///
    /// Manual selection is untouched by any of this — a viewer who picks a PGS
    /// track still gets a burn, at source height.
    static func automaticSubtitleIndex(
        _ tracks: [SubtitleTrack],
        preferredLanguage: String
    ) -> Int? {
        guard preferredLanguage.lowercased() != "off" else { return nil }
        let matching = tracks.filter {
            languageCode($0.language) == languageCode(preferredLanguage)
        }
        if let forcedIndex = matching.first(where: { isForcedSubtitle($0) })?.index {
            return forcedIndex
        }
        return matching.first(where: { $0.default && $0.isNativeHLS })?.index
    }

    /// Forced-ness has two signals because muxes disagree about which to set.
    static func isForcedSubtitle(_ track: SubtitleTrack) -> Bool {
        track.forced || track.title?.localizedCaseInsensitiveContains("forced") == true
    }

    /// Collapse the common ISO 639-1 and 639-2/B spellings used by settings
    /// and ffprobe into the same comparison key.
    private static func languageCode(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let code = raw
            .lowercased()
            .replacingOccurrences(of: "_", with: "-")
            .split(separator: "-")
            .first
            .map(String.init) ?? ""
        let aliases = [
            "eng": "en", "jpn": "ja", "spa": "es", "fre": "fr", "fra": "fr",
            "ger": "de", "deu": "de", "ita": "it", "por": "pt", "kor": "ko",
            "chi": "zh", "zho": "zh", "rus": "ru", "hin": "hi", "ara": "ar",
        ]
        return aliases[code] ?? (code.isEmpty ? nil : code)
    }

    private static func legacyMode(_ method: String) -> String {
        switch method {
        case "direct_play": return "direct"
        case "remux": return "remux"
        default: return "transcode"
        }
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
                self?.updateNowPlaying()
            }
            return .success
        }))
        remoteTargets.append((commands.pauseCommand, commands.pauseCommand.addTarget { [weak self] _ in
            Task { @MainActor in
                self?.player.pause()
                self?.isPlaying = false
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
