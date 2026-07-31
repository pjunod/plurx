import AVKit
import Combine
import Foundation
#if os(iOS)
import MediaPlayer
#endif

/// Drives one AVPlayer and executes the server-owned delivery plan. It also
/// supplies the controls AVPlayer withholds for a growing EVENT playlist: an
/// explicit on-demand timeline, reliable play/pause commands, server playback
/// telemetry, and stream restarts for subtitle/audio/quality changes.
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
    private var statusTask: Task<Void, Never>?
    private var started = false
    private var sessionId: String?
    private var lastReportedMs = 0
    private var usesDirectTimeline = false
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
        if selectedSubtitle != nil { return "Transcode · subtitle burn-in" }
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
        Task { await reopen(at: realPositionMs()) }
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
            // The server's default flag already contains its Auto/Always/Off
            // subtitle rule. A local explicit Off remains an override.
            if model.subLang != "off" {
                selectedSubtitle = decision.subtitles?.first(where: { $0.default })?.index
            }
            try await open(decision: decision, at: startMs)
        } catch {
            fail(error)
        }
    }

    private func reopen(at position: Int) async {
        guard let decision else { return }
        do {
            try await open(decision: decision, at: position)
        } catch {
            fail(error)
        }
    }

    private func open(decision: Decision, at startMs: Int) async throws {
        guard let model else { return }
        isChangingStream = true
        playbackError = nil
        player.pause()
        await releaseCurrentSession()

        let normalMode = decision.delivery?.mode ?? Self.legacyMode(decision.method)
        let forceTranscode = selectedSubtitle != nil || selectedHeight != nil
        let customAudio = audioOverride != nil
        let direct = normalMode == "direct" && !forceTranscode && !customAudio
        let url: URL?
        var seekAfterAttach: Int?

        if direct {
            baseMs = 0
            usesDirectTimeline = true
            isVOD = true
            encoder = nil
            sessionStatus = nil
            url = Session.shared.mediaURL(decision.delivery?.url ?? decision.playUrl)
            if startMs > 0 { seekAfterAttach = startMs }
        } else {
            let copy = !forceTranscode && (normalMode == "remux" || customAudio)
            let chosenAudio = audioOverride
            let aac = copy ? needsAAC(audioIndex: chosenAudio, decision: decision)
                : nil
            // A subtitle burn on a file the device could otherwise take keeps
            // source resolution. A genuine transcode still lets server Auto
            // choose its rung unless the viewer selected one explicitly.
            let burnHeight = selectedSubtitle != nil && normalMode != "transcode"
                ? decision.source?.height : nil
            let body = CreateSessionRequest(
                playbackId: playbackId,
                height: selectedHeight ?? burnHeight,
                start: Double(startMs) / 1000.0,
                audio: chosenAudio,
                subtitleBurn: selectedSubtitle,
                copy: copy ? true : nil,
                aac: copy ? aac : nil,
                preserveDolbyVision: copy ? (decision.preserveDolbyVision ?? false) : nil
            )
            let hls = try await model.createHlsSession(fileId: fileId, body: body)
            sessionId = hls.sessionId
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
        player.replaceCurrentItem(with: item)
        if let seekAfterAttach { await seekWhenReady(item, ms: seekAfterAttach) }
        player.play()
        isPlaying = true
        currentMs = startMs
        isChangingStream = false
        updateNowPlaying()
    }

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
        failed = player.currentItem == nil
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
                guard let self else { return }
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

    private func report(_ position: Int) {
        guard position > 0 else { return }
        let duration = knownDurationMs > 0 ? knownDurationMs : nil
        let itemId = itemId
        let model = model
        Task { await model?.reportProgress(itemId: itemId, positionMs: position, durationMs: duration) }
    }

    private func seekWhenReady(_ item: AVPlayerItem, ms: Int) async {
        for await status in item.publisher(for: \.status).values {
            if status == .readyToPlay {
                _ = await player.seek(
                    to: CMTime(seconds: Double(ms) / 1000.0, preferredTimescale: 600),
                    toleranceBefore: .zero,
                    toleranceAfter: .zero
                )
                return
            }
            if status == .failed { return }
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
