import AVKit
import Combine
import Foundation

/// Drives one AVPlayer for a title. plurx's two delivery shapes map cleanly to
/// Apple:
///  - `direct_play` → the original file at `/files/{id}/direct?token=…`, which
///    AVPlayer range-seeks natively (base = 0).
///  - remux/transcode → a native HLS session started at the resume point; the
///    capability-authed playlist needs no token, and `startSeconds` becomes the
///    base offset so the true timeline position is reported for the scrobble.
@MainActor
final class PlayerController: ObservableObject {
    let player = AVPlayer()
    @Published var failed = false

    private var baseMs = 0
    private var durationMs = 0
    private var itemId = 0
    private weak var model: AppModel?
    private var timeObserver: Any?
    private var started = false

    func start(model: AppModel, itemId: Int, fileId: Int, startMs: Int, durationMs: Int, title: String) {
        guard !started else { return }
        started = true
        self.model = model
        self.itemId = itemId
        self.durationMs = durationMs

        #if os(iOS)
        try? AVAudioSession.sharedInstance().setCategory(.playback)
        try? AVAudioSession.sharedInstance().setActive(true)
        #endif

        applyLanguagePrefs(audio: model.audioLang, sub: model.subLang)
        player.appliesMediaSelectionCriteriaAutomatically = true

        Task { await load(fileId: fileId, startMs: startMs, title: title) }
    }

    private func load(fileId: Int, startMs: Int, title: String) async {
        guard let model else { return }
        do {
            let decision = try await model.decision(fileId: fileId)
            let url: URL?
            let direct = decision.method == "direct_play"
            if direct {
                baseMs = 0
                url = Session.shared.mediaURL(decision.playUrl)
            } else {
                let hls = try await model.hlsStart(
                    fileId: fileId, height: 1080, start: Double(startMs) / 1000.0, audio: nil
                )
                baseMs = Int((hls.startSeconds ?? Double(startMs) / 1000.0) * 1000)
                url = Session.shared.url(hls.playlistUrl)   // capability auth — no token
            }
            guard let url else { failed = true; return }

            let item = AVPlayerItem(url: url)
            item.externalMetadata = [titleMetadata(title)]
            player.replaceCurrentItem(with: item)
            player.play()

            if direct, startMs > 0 { await seekWhenReady(item, ms: startMs) }
            addPeriodicObserver()
        } catch {
            failed = true
        }
    }

    /// True timeline position (ms), base offset included for the HLS path.
    func realPositionMs() -> Int {
        let secs = player.currentTime().seconds
        let pos = secs.isFinite ? Int(secs * 1000) : 0
        return baseMs + max(pos, 0)
    }

    /// Post the final position and tear down (drives the server-side Trakt scrobble).
    func stop() {
        if let timeObserver { player.removeTimeObserver(timeObserver); self.timeObserver = nil }
        let pos = realPositionMs()
        player.pause()
        if pos > 0 {
            let dur = durationMs > 0 ? durationMs : nil
            let id = itemId
            let m = model
            Task { await m?.reportProgress(itemId: id, positionMs: pos, durationMs: dur) }
        }
    }

    // MARK: - Internals

    private func addPeriodicObserver() {
        let interval = CMTime(seconds: 10, preferredTimescale: 1)
        timeObserver = player.addPeriodicTimeObserver(forInterval: interval, queue: .main) { [weak self] _ in
            // The observer fires on `.main`; assert that isolation for the actor-bound state.
            MainActor.assumeIsolated {
                guard let self, self.player.timeControlStatus == .playing else { return }
                let pos = self.realPositionMs()
                let dur = self.durationMs > 0 ? self.durationMs : nil
                let id = self.itemId
                let m = self.model
                Task { await m?.reportProgress(itemId: id, positionMs: pos, durationMs: dur) }
            }
        }
    }

    private func seekWhenReady(_ item: AVPlayerItem, ms: Int) async {
        for await status in item.publisher(for: \.status).values {
            if status == .readyToPlay {
                _ = await player.seek(to: CMTime(seconds: Double(ms) / 1000.0, preferredTimescale: 600))
                return
            } else if status == .failed {
                return
            }
        }
    }

    private func applyLanguagePrefs(audio: String, sub: String) {
        player.setMediaSelectionCriteria(
            AVPlayerMediaSelectionCriteria(preferredLanguages: bcp47(audio), preferredMediaCharacteristics: nil),
            forMediaCharacteristic: .audible
        )
        let subLangs = sub == "off" ? [] : bcp47(sub)
        player.setMediaSelectionCriteria(
            AVPlayerMediaSelectionCriteria(preferredLanguages: subLangs, preferredMediaCharacteristics: nil),
            forMediaCharacteristic: .legible
        )
    }

    private func titleMetadata(_ title: String) -> AVMetadataItem {
        let m = AVMutableMetadataItem()
        m.identifier = .commonIdentifierTitle
        m.value = title as NSString
        m.extendedLanguageTag = "und"
        return m
    }

    /// Map the server's ISO 639-2/B codes to BCP-47 primaries AVFoundation
    /// matches on, keeping the original as a fallback.
    private func bcp47(_ code: String) -> [String] {
        let map = [
            "eng": "en", "jpn": "ja", "spa": "es", "fre": "fr", "ger": "de", "ita": "it",
            "por": "pt", "kor": "ko", "chi": "zh", "rus": "ru", "hin": "hi", "ara": "ar",
        ]
        if let two = map[code] { return [two, code] }
        return [code]
    }
}
