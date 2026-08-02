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
    case reopen
}

/// Drives one AVPlayer and executes the server-owned delivery plan. It also
/// supplies the controls AVPlayer withholds for a growing EVENT playlist: an
/// explicit on-demand timeline, reliable play/pause commands, server playback
/// telemetry, and stream restarts for audio, quality, and burn-only subtitle
/// changes. Ordinary text subtitles switch through AVPlayer media selection.
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
    /// True while the player is on the raw file instead of an HLS session.
    /// P2-7: the first native subtitle selection has to create the session.
    private var isDirectPlayback = false
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

        Task { await load(startMs: startMs) }
    }

    func togglePlayPause() {
        if player.timeControlStatus == .playing || player.rate > 0 {
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
        let route = Self.subtitleSelectionRoute(
            for: index,
            tracks: subtitles,
            activeBurn: activeBurnedSubtitle,
            isDirectPlayback: isDirectPlayback
        )
        Task { await applySubtitleSelection(index, route: route) }
    }

    private func applySubtitleSelection(_ index: Int?, route: SubtitleSelectionRoute) async {
        switch route {
        case .reopen:
            await reopen(at: realPositionMs())
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
            // `default` on a decision track is the server's own shared-policy
            // pick, not the muxer's flag (crates/plurxd http/stream.rs
            // overwrites it from `select_tracks`), so the audio it names is
            // the audio that will actually play.
            let chosenAudio = decision.audio?.first(where: { $0.default })
            selectedAudio = chosenAudio?.index
            // Container defaults describe the muxer's primary language, not
            // this viewer. Choose only within the preferred language, prefer a
            // forced/narrative track before a full subtitle track, and honor
            // the server's Auto rule against the audio language.
            selectedSubtitle = Self.automaticSubtitleIndex(
                decision.subtitles ?? [],
                preferredLanguage: model.subLang,
                audioLanguage: chosenAudio?.language
            )
            try await open(decision: decision, at: startMs)
        } catch {
            fail(error)
        }
    }

    private func reopen(at position: Int) async {
        // A growing EVENT playlist can momentarily announce its current end,
        // and several UI actions can also request a restart. Never overlap two
        // server-session replacements: they share a playback ID, so the newer
        // request intentionally removes the older session and can otherwise
        // strand AVPlayer on a URL the server has just deleted.
        guard let decision, started, !isChangingStream else { return }
        do {
            try await open(decision: decision, at: position)
        } catch {
            fail(error)
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
        player.pause()
        await releaseCurrentSession()
        guard !isSuperseded(generation) else { return }
        guard started else {
            isChangingStream = false
            return
        }

        let normalMode = decision.delivery?.mode ?? Self.legacyMode(decision.method)
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
        canRetryCurrentItemWithTranscode = normalMode != "transcode" && !forceTranscode && !customAudio
        // P2-7, decided by Paul on 2026-08-02 (plan §2.5): stay direct until
        // the first native selection. Merely *having* native text tracks no
        // longer abolishes true direct play — every such file used to become a
        // copy session, and on Bedroom that path degrades to a compatibility
        // transcode. Entering the session is deferred to the moment a native
        // subtitle is actually chosen, which costs exactly one reopen there
        // (routed explicitly by `subtitleSelectionRoute`).
        let direct = normalMode == "direct" && !forceTranscode && !customAudio && nativeSubtitle == nil
        let url: URL?
        var seekAfterAttach: Int?

        if direct {
            activeBurnedSubtitle = nil
            isDirectPlayback = true
            baseMs = 0
            usesDirectTimeline = true
            isVOD = true
            encoder = nil
            sessionStatus = nil
            url = Session.shared.mediaURL(decision.delivery?.url ?? decision.playUrl)
            if startMs > 0 { seekAfterAttach = startMs }
        } else {
            let copy = !forceTranscode
                && (normalMode == "direct" || normalMode == "remux" || customAudio)
            let chosenAudio = audioOverride
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
                preserveDolbyVision: copy ? (decision.preserveDolbyVision ?? false) : nil
            )
            let hls: HlsStart
            do {
                hls = try await model.createHlsSession(fileId: fileId, body: body)
            } catch {
                // A superseded attempt must not report its own failure over
                // the newer open's state (P2-6).
                if isSuperseded(generation) { return }
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

        guard let url else { throw APIError.badURL }
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
        failed = false
        isChangingStream = false
        updateNowPlaying()
        // P1-2: `reopen()` refuses to overlap an in-flight open and this open
        // applied the selection it captured at entry, so a track picked during
        // a cold extraction would otherwise show a checkmark forever while the
        // stream renders the old choice. Apply whatever the viewer last chose.
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

    private func releaseCurrentSession() async {
        statusTask?.cancel()
        statusTask = nil
        sessionStatus = nil
        guard let sessionId else { return }
        self.sessionId = nil
        await model?.endHlsSession(sessionId)
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
                // The last rate the viewer was genuinely playing at, so a
                // pause at 1.5× is restored as 1.5× and not as the 0 the
                // transport reports while paused (P2-5).
                if self.isPlaying && self.player.rate > 0 {
                    self.preferredRate = self.player.rate
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
                if Self.shouldRetryWithCompatibilityTranscode(
                    canRetry: self.canRetryCurrentItemWithTranscode,
                    alreadyAttempted: self.compatibilityFallbackAttempted
                ), self.started {
                    self.compatibilityFallbackAttempted = true
                    self.forceCompatibilityTranscode = true
                    self.canRetryCurrentItemWithTranscode = false
                    self.isChangingStream = false
                    self.playbackError = "The original stream could not open. Retrying a compatible stream…"
                    // P2-6: this item is already dead, so its `currentTime()`
                    // is 0 or invalid and a VOD/direct retry would silently
                    // restart the film at 0:00. The last position the periodic
                    // observer saw is the truthful retry point. The transport
                    // intent survives in `wantsPlayback`, so a viewer who was
                    // paused when the item failed stays paused.
                    await self.reopen(
                        at: Self.compatibilityRetryPositionMs(lastObservedMs: self.currentMs)
                    )
                    return
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

    /// Position in the HLS master rendition order. The server advertises only
    /// native tracks and preserves source order, so this remains stable even
    /// when bitmap and styled tracks are interleaved in the menu.
    static func nativeSubtitleOrdinal(_ index: Int, in tracks: [SubtitleTrack]) -> Int? {
        tracks.filter(\.isNativeHLS).firstIndex(where: { $0.index == index })
    }

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
        playbackError = "That subtitle track could not be turned on."
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

    /// Pick the subtitle the player may enable automatically. Never fall back
    /// to a flagged container default in another language: an Italian-first
    /// mux can otherwise burn Italian captions while English audio is playing.
    /// Some release muxes omit the forced disposition and retain only a
    /// "Forced" title, so both signals are meaningful.
    ///
    /// Owner policy (plan §3.3, decided 2026-08-02): **automatic selection
    /// must never start a burn — except a forced track, which may, always at
    /// source height.** Every arm below other than the forced language match
    /// is therefore restricted to native-HLS formats, and a language whose
    /// only matches are PGS/VobSub/ASS selects nothing at all rather than
    /// silently spawning a video encoder on every play (P0-1).
    ///
    /// `audioLanguage` is the language of the audio track that is about to
    /// play. The server's shared policy defaults to `SubMode::Auto`
    /// (crates/plurx-core/src/tracks.rs `select_tracks`), where audio already
    /// speaking the preferred subtitle language leaves only the floor — a
    /// forced overlay or a flagged default — eligible. Ignoring that was a
    /// real divergence: it turned on a full English subtitle track under
    /// English audio on every play, which is also what made P2-7's direct-play
    /// survival worthless in the common case.
    static func automaticSubtitleIndex(
        _ tracks: [SubtitleTrack],
        preferredLanguage: String,
        audioLanguage: String? = nil
    ) -> Int? {
        guard preferredLanguage.lowercased() != "off" else { return nil }
        // A blank preference must not resolve to nil and thereby match every
        // *untagged* track through the language arms, whose forced test is
        // format-agnostic — that is the one place a burn can start.
        guard let preferred = languageCode(preferredLanguage) else { return nil }
        let audioSpeaksPreferred = audioLanguage.map { languageCode($0) == preferred } ?? false
        let matching = tracks.filter { languageCode($0.language) == preferred }
        // P2-9: the server's shared policy keeps untagged tracks eligible
        // ("Untagged tracks remain eligible", crates/plurx-core/src/tracks.rs
        // `forced_or_default`), because a missing tag is not contrary
        // information. Mirror that, after every genuine language match, and
        // only for native formats: a track that is neither known to be in the
        // viewer's language nor cheap to show is not worth a burn.
        let untagged = tracks.filter { $0.language == nil || languageCode($0.language) == nil }
        if let forced = matching.first(where: { isForcedSubtitle($0) })?.index { return forced }
        if let flagged = matching.first(where: { $0.default && $0.isNativeHLS })?.index {
            return flagged
        }
        if !audioSpeaksPreferred,
           let first = matching.first(where: { $0.isNativeHLS })?.index {
            return first
        }
        if let forced = untagged.first(where: { isForcedSubtitle($0) && $0.isNativeHLS })?.index {
            return forced
        }
        return untagged.first(where: { $0.default && $0.isNativeHLS })?.index
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

    /// What one subtitle selection costs. A burn — or leaving one — replaces
    /// the video frames, and P2-7's direct-play → session boundary needs the
    /// session to exist before a native rendition can be selected at all.
    static func subtitleSelectionRoute(
        for index: Int?,
        tracks: [SubtitleTrack],
        activeBurn: Int?,
        isDirectPlayback: Bool
    ) -> SubtitleSelectionRoute {
        let needsBurn = index.map { subtitleRequiresBurn($0, in: tracks) } ?? false
        let leavesDirectPlay = isDirectPlayback && index != nil
        return needsBurn || activeBurn != nil || leavesDirectPlay ? .reopen : .mediaSelection
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
        isDirectPlayback: Bool
    ) -> SubtitleSelectionRoute? {
        guard applied != current else { return nil }
        return subtitleSelectionRoute(
            for: current,
            tracks: tracks,
            activeBurn: activeBurn,
            isDirectPlayback: isDirectPlayback
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
