import SwiftUI

#if os(tvOS)
private enum PlayerControl: Hashable {
    case close
    case progress
    case marker
    case skipBack
    case playPause
    case skipForward
    case audio
    case subtitles
    case quality
    case autoplay
    case stats
}
#endif

/// Full-screen Apple player with an explicit on-demand transport. AVPlayer
/// sees a growing plurx HLS playlist as an EVENT stream while the server is
/// producing it, so relying on the system overlay alone labels a movie LIVE
/// and can replace Pause with Stop. The controls here use the known film
/// runtime and work for direct, remux, and transcode delivery alike.
struct PlayerView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    let itemId: Int
    let fileId: Int
    let startMs: Int
    let durationMs: Int
    let title: String
    var onPlayNext: ((PlayContext) -> Void)?

    @StateObject private var controller = PlayerController()
    @State private var showStats = false
    @State private var findingNext = false
    @State private var isScrubbing = false
    @State private var scrubMs = 0.0
    @State private var controlsVisible = true
    @State private var autoHideGeneration = 0
    #if os(tvOS)
    @FocusState private var focusedControl: PlayerControl?
    #endif

    var body: some View {
        ZStack(alignment: .topLeading) {
            Color.black.ignoresSafeArea()

            PlayerSurface(player: controller.player)
                .ignoresSafeArea()

            #if os(iOS)
            Color.clear
                .contentShape(Rectangle())
                .ignoresSafeArea()
                .onTapGesture { toggleControls() }
                .accessibilityLabel("Show or hide playback controls")
            #endif

            if controller.failed {
                failureView
            } else if shouldShowControls {
                VStack(spacing: 0) {
                    HStack(alignment: .top) {
                        #if os(iOS)
                        closeButton
                        #endif
                        Spacer()
                        if showStats {
                            PlaybackStatsView(controller: controller)
                                .transition(.opacity.combined(with: .move(edge: .trailing)))
                        }
                    }
                    Spacer()
                    playbackControls
                }
                .padding(20)
                .transition(.opacity)
            }

            if controller.isChangingStream {
                ProgressView()
                    .controlSize(.large)
                    .tint(.white)
                    .padding(18)
                    .background(.ultraThinMaterial, in: Circle())
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }

            if findingNext {
                VStack(spacing: 10) {
                    ProgressView().tint(.white)
                    Text("Up next…")
                        .font(.system(.callout, design: .monospaced))
                        .foregroundColor(.white)
                }
                .padding(18)
                .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }

            if let error = controller.playbackError, !controller.failed {
                Text(error)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(.white)
                    .padding(10)
                    .background(Palette.accent.opacity(0.9), in: RoundedRectangle(cornerRadius: 8))
                    .frame(maxWidth: .infinity, alignment: .top)
                    .padding(.top, 20)
                    .padding(.horizontal, 80)
            }
        }
        .task {
            controller.start(
                model: model,
                itemId: itemId,
                fileId: fileId,
                startMs: startMs,
                durationMs: durationMs,
                title: title
            )
            #if os(tvOS)
            try? await Task.sleep(nanoseconds: 100_000_000)
            focusedControl = .playPause
            #endif
        }
        .onDisappear { controller.stop() }
        .task(id: autoHideGeneration) {
            #if os(iOS)
            guard controlsVisible,
                  controller.isPlaying,
                  !showStats,
                  !isScrubbing,
                  !controller.isChangingStream else { return }
            try? await Task.sleep(nanoseconds: 3_500_000_000)
            guard !Task.isCancelled,
                  controller.isPlaying,
                  !showStats,
                  !isScrubbing,
                  !controller.isChangingStream else { return }
            withAnimation(.easeOut(duration: 0.2)) { controlsVisible = false }
            #endif
        }
        .onChange(of: controller.isPlaying) { _, _ in revealControls() }
        .onChange(of: controller.isChangingStream) { _, _ in revealControls() }
        .onChange(of: showStats) { _, _ in restartAutoHideTimer() }
        .onChange(of: isScrubbing) { _, _ in restartAutoHideTimer() }
        .onChange(of: controller.finished) { _, finished in
            guard finished, model.autoplay else { return }
            findingNext = true
            Task {
                if let next = await model.nextEpisode(after: itemId) {
                    controller.stop()
                    onPlayNext?(next)
                } else {
                    findingNext = false
                }
            }
        }
        #if os(tvOS)
        .onExitCommand { dismiss() }
        #endif
    }

    private var shouldShowControls: Bool {
        #if os(tvOS)
        true
        #else
        controlsVisible
        #endif
    }

    private func toggleControls() {
        #if os(iOS)
        withAnimation(.easeInOut(duration: 0.2)) {
            if controlsVisible {
                controlsVisible = false
                showStats = false
            } else {
                controlsVisible = true
            }
        }
        restartAutoHideTimer()
        #endif
    }

    private func revealControls() {
        #if os(iOS)
        withAnimation(.easeInOut(duration: 0.2)) { controlsVisible = true }
        restartAutoHideTimer()
        #endif
    }

    private func restartAutoHideTimer() {
        #if os(iOS)
        autoHideGeneration &+= 1
        #endif
    }

    private var failureView: some View {
        VStack(spacing: 14) {
            Text("Couldn't start playback.")
                .font(.system(.body, design: .monospaced))
                .foregroundColor(.white)
            if let error = controller.playbackError {
                Text(error)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(Palette.muted)
                    .multilineTextAlignment(.center)
            }
            Button("Close") { dismiss() }
                .buttonStyle(.borderedProminent)
                .tint(Palette.accent)
                #if os(tvOS)
                .focused($focusedControl, equals: .close)
                #endif
        }
        .padding(30)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
    }

    #if os(iOS)
    private var closeButton: some View {
        Button { dismiss() } label: {
            Image(systemName: "xmark.circle.fill")
                .font(.largeTitle)
                .foregroundStyle(.white.opacity(0.9))
        }
        .buttonStyle(.plain)
    }
    #endif

    private var playbackControls: some View {
        VStack(spacing: 10) {
            if controller.knownDurationMs > 0 {
                HStack(spacing: 10) {
                    Text(formatTime(Int(isScrubbing ? scrubMs : Double(controller.currentMs))))
                        .fixedSize()
                    #if os(tvOS)
                    tvProgressBar
                    #else
                    Slider(
                        value: Binding(
                            get: { isScrubbing ? scrubMs : Double(controller.currentMs) },
                            set: { scrubMs = $0 }
                        ),
                        in: 0...Double(controller.knownDurationMs),
                        onEditingChanged: { editing in
                            if editing {
                                scrubMs = Double(controller.currentMs)
                                isScrubbing = true
                            } else {
                                isScrubbing = false
                                controller.seek(toMs: Int(scrubMs))
                            }
                        }
                    )
                    .tint(Palette.accent)
                    #endif
                    Text(formatTime(controller.knownDurationMs))
                        .fixedSize()
                }
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(.white)
            }

            if let marker = controller.activeMarker {
                #if os(tvOS)
                HStack {
                    Spacer(minLength: 0)
                    markerButton(marker)
                        .fixedSize(horizontal: true, vertical: false)
                }
                #else
                markerButton(marker)
                #endif
            }

            #if os(tvOS)
            expandedControlRow
            #else
            ViewThatFits(in: .horizontal) {
                expandedControlRow
                    .fixedSize(horizontal: true, vertical: false)
                compactControlRow
            }
            .frame(maxWidth: .infinity)
            #endif
        }
        .font(.title3)
        .buttonStyle(.bordered)
        #if os(tvOS)
        .tint(Palette.surfaceHi)
        .foregroundStyle(.white)
        #else
        .tint(.white)
        #endif
        .padding(12)
        .frame(maxWidth: .infinity)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
    }

    private func markerButton(_ marker: Marker) -> some View {
        Button {
            controller.skipActiveMarker()
            revealControls()
        } label: {
            Label(marker.label, systemImage: "forward.end.fill")
                .font(.system(.caption, design: .monospaced))
                #if os(iOS)
                .frame(maxWidth: .infinity)
                #endif
        }
        #if os(tvOS)
        .buttonStyle(TVReadableButtonStyle(prominent: true))
        .focused($focusedControl, equals: .marker)
        #else
        .buttonStyle(.borderedProminent)
        .tint(Palette.accent)
        #endif
    }

    private var expandedControlRow: some View {
        HStack(spacing: 12) {
            skipBackButton
            playPauseButton
            skipForwardButton
            Spacer(minLength: 4)
            if controller.audioTracks.count > 1 { audioMenu }
            if !controller.subtitles.isEmpty { subtitleMenu }
            if !controller.qualityRungs.isEmpty { qualityMenu }
            autoplayButton
            statsButton
        }
    }

    #if os(iOS)
    private var compactControlRow: some View {
        HStack(spacing: 8) {
            skipBackButton
            playPauseButton
            skipForwardButton
            Spacer(minLength: 4)
            moreMenu
        }
        .frame(maxWidth: .infinity)
    }
    #endif

    private var skipBackButton: some View {
        Button {
            controller.skip(seconds: -10)
            revealControls()
        } label: {
            Image(systemName: "gobackward.10")
        }
        .accessibilityLabel("Back 10 seconds")
        #if os(tvOS)
        .focused($focusedControl, equals: .skipBack)
        #endif
    }

    private var playPauseButton: some View {
        Button {
            controller.togglePlayPause()
            revealControls()
        } label: {
            Image(systemName: controller.isPlaying ? "pause.fill" : "play.fill")
                .frame(minWidth: 20)
        }
        .accessibilityLabel(controller.isPlaying ? "Pause" : "Play")
        #if os(tvOS)
        .focused($focusedControl, equals: .playPause)
        #endif
    }

    private var skipForwardButton: some View {
        Button {
            controller.skip(seconds: 10)
            revealControls()
        } label: {
            Image(systemName: "goforward.10")
        }
        .accessibilityLabel("Forward 10 seconds")
        #if os(tvOS)
        .focused($focusedControl, equals: .skipForward)
        #endif
    }

    private var autoplayButton: some View {
        Button {
            model.setAutoplay(!model.autoplay)
            revealControls()
        } label: {
            Image(systemName: "play.square.stack.fill")
                .foregroundStyle(model.autoplay ? Palette.accent : .white)
        }
        .accessibilityLabel(model.autoplay ? "Autoplay next on" : "Autoplay next off")
        #if os(tvOS)
        .focused($focusedControl, equals: .autoplay)
        #endif
    }

    private var statsButton: some View {
        Button {
            withAnimation { showStats.toggle() }
            revealControls()
        } label: {
            Image(systemName: "info.circle.fill")
                .foregroundStyle(showStats ? Palette.accent : .white)
        }
        .accessibilityLabel("Playback info")
        #if os(tvOS)
        .focused($focusedControl, equals: .stats)
        #endif
    }

    #if os(iOS)
    private var moreMenu: some View {
        Menu {
            if controller.audioTracks.count > 1 {
                Menu("Audio") { audioChoices }
            }
            if !controller.subtitles.isEmpty {
                Menu("Subtitles") { subtitleChoices }
            }
            if !controller.qualityRungs.isEmpty {
                Menu("Quality") { qualityChoices }
            }
            Divider()
            Button {
                model.setAutoplay(!model.autoplay)
                revealControls()
            } label: {
                Label(model.autoplay ? "Turn off autoplay" : "Turn on autoplay",
                      systemImage: "play.square.stack.fill")
            }
            Button {
                withAnimation { showStats.toggle() }
                revealControls()
            } label: {
                Label(showStats ? "Hide playback info" : "Playback info",
                      systemImage: "info.circle.fill")
            }
        } label: {
            Image(systemName: "ellipsis.circle.fill")
        }
        .accessibilityLabel("More playback options")
        .simultaneousGesture(TapGesture().onEnded { revealControls() })
    }
    #endif

    #if os(tvOS)
    /// SwiftUI's Slider is unavailable on tvOS. This focusable bar uses the
    /// Siri Remote's left/right commands to move through the same absolute
    /// film timeline in 30-second steps.
    private var tvProgressBar: some View {
        Button {} label: {
            GeometryReader { geometry in
                let fraction = controller.knownDurationMs > 0
                    ? min(max(Double(controller.currentMs) / Double(controller.knownDurationMs), 0), 1)
                    : 0
                ZStack(alignment: .leading) {
                    Capsule().fill(.white.opacity(0.25))
                    Capsule()
                        .fill(Palette.accent)
                        .frame(width: geometry.size.width * fraction)
                }
            }
            .frame(height: 8)
        }
        .buttonStyle(.plain)
        .focused($focusedControl, equals: .progress)
        .onMoveCommand { direction in
            switch direction {
            case .left: controller.skip(seconds: -30)
            case .right: controller.skip(seconds: 30)
            default: break
            }
        }
        .accessibilityLabel("Playback position. Left or right seeks 30 seconds.")
    }
    #endif

    private var audioMenu: some View {
        Menu { audioChoices } label: {
            Image(systemName: "speaker.wave.2.fill")
        }
        .accessibilityLabel("Audio track")
        #if os(tvOS)
        .focused($focusedControl, equals: .audio)
        #endif
    }

    private var subtitleMenu: some View {
        Menu { subtitleChoices } label: {
            Image(systemName: "captions.bubble.fill")
                .foregroundStyle(controller.selectedSubtitle == nil ? .white : Palette.accent)
        }
        .accessibilityLabel("Subtitles")
        #if os(tvOS)
        .focused($focusedControl, equals: .subtitles)
        #endif
    }

    private var qualityMenu: some View {
        Menu { qualityChoices } label: {
            Image(systemName: "slider.horizontal.3")
        }
        .accessibilityLabel("Playback quality")
        #if os(tvOS)
        .focused($focusedControl, equals: .quality)
        #endif
    }

    @ViewBuilder
    private var audioChoices: some View {
        ForEach(controller.audioTracks) { track in
            Button {
                controller.selectAudio(track.index)
                revealControls()
            } label: {
                Label(
                    audioLabel(track),
                    systemImage: controller.selectedAudio == track.index
                        ? "checkmark"
                        : "speaker.wave.2"
                )
            }
        }
    }

    @ViewBuilder
    private var subtitleChoices: some View {
        Button {
            controller.selectSubtitle(nil)
            revealControls()
        } label: {
            Label(
                "Off",
                systemImage: controller.selectedSubtitle == nil
                    ? "checkmark"
                    : "captions.bubble"
            )
        }
        ForEach(controller.subtitles) { track in
            Button {
                controller.selectSubtitle(track.index)
                revealControls()
            } label: {
                Label(
                    subtitleLabel(track),
                    systemImage: controller.selectedSubtitle == track.index
                        ? "checkmark"
                        : "captions.bubble"
                )
            }
        }
    }

    @ViewBuilder
    private var qualityChoices: some View {
        Button {
            controller.selectQuality(nil)
            revealControls()
        } label: {
            Label(
                "Auto",
                systemImage: controller.selectedHeight == nil
                    ? "checkmark"
                    : "wand.and.stars"
            )
        }
        ForEach(controller.qualityRungs) { rung in
            Button {
                controller.selectQuality(rung.height)
                revealControls()
            } label: {
                Label(
                    "\(rung.height)p · \(rung.totalKbps / 1000) Mb/s",
                    systemImage: controller.selectedHeight == rung.height
                        ? "checkmark"
                        : "rectangle.inset.filled"
                )
            }
        }
    }

    private func audioLabel(_ track: AudioTrack) -> String {
        [track.title, languageName(track.language), track.codec.uppercased(),
         track.channels.map { "\($0) ch" }]
            .compactMap { $0 }
            .joined(separator: " · ")
    }

    private func subtitleLabel(_ track: SubtitleTrack) -> String {
        var parts = [track.title, languageName(track.language), track.codec.uppercased()]
            .compactMap { $0 }
        if track.forced { parts.append("Forced") }
        if !track.text { parts.append("Burn-in") }
        return parts.joined(separator: " · ")
    }

    private func languageName(_ code: String?) -> String? {
        guard let code else { return nil }
        let names = [
            "eng": "English", "en": "English", "jpn": "Japanese", "ja": "Japanese",
            "spa": "Spanish", "es": "Spanish", "fre": "French", "fr": "French",
            "ger": "German", "de": "German", "ita": "Italian", "por": "Portuguese",
            "kor": "Korean", "chi": "Chinese", "rus": "Russian",
        ]
        return names[code.lowercased()] ?? code.uppercased()
    }
}

/// Compact Apple equivalent of the web player's playback-info panel. It uses
/// the same decision and HLS status contracts, so a buffering report can say
/// whether the link or the encoder is actually falling behind.
private struct PlaybackStatsView: View {
    @ObservedObject var controller: PlayerController

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 7) {
                Text("Playback info")
                    .font(.system(.headline, design: .monospaced))
                    .foregroundColor(.white)
                Divider().overlay(Palette.outline)
                row("Method", controller.methodLabel)
                row("Position", "\(formatTime(controller.currentMs)) / \(formatTime(controller.knownDurationMs))")
                sourceRows
                outputRows
                serverRows
                if let reasons = controller.decision?.reasons, !reasons.isEmpty {
                    row("Reason", reasons.joined(separator: "; "))
                }
            }
            .padding(14)
        }
        .frame(maxWidth: 430, maxHeight: 430)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    @ViewBuilder
    private var sourceRows: some View {
        if let source = controller.decision?.source {
            let resolution = source.width.flatMap { width in source.height.map { "\(width)×\($0)" } } ?? "—"
            let video = [source.videoCodec?.uppercased(), source.bitDepth.map { "\($0)-bit" },
                         source.hdrFormat ?? source.hdr?.uppercased()]
                .compactMap { $0 }.joined(separator: " · ")
            row("Source", "\(resolution) · \(video.isEmpty ? "—" : video)")
            row("Container", source.container?.uppercased() ?? "—")
            if let bitrate = source.bitrate { row("Source rate", bitRate(bitrate)) }
        }
    }

    @ViewBuilder
    private var outputRows: some View {
        let size = controller.presentationSize
        if size.width > 0 && size.height > 0 {
            row("Output", "\(Int(size.width))×\(Int(size.height))")
        }
        if let bitrate = controller.observedBitrate, bitrate > 0 {
            row("Observed rate", bitRate(Int(bitrate)))
        } else if let bitrate = controller.indicatedBitrate, bitrate > 0 {
            row("Stream rate", bitRate(Int(bitrate)))
        }
        if let stalls = controller.stalls { row("Player stalls", String(stalls)) }
    }

    @ViewBuilder
    private var serverRows: some View {
        if controller.isVOD && controller.methodLabel.contains("cached") {
            row("Server", "Already transcoded · served from cache")
        } else if let status = controller.sessionStatus {
            if let encoder = status.encoder ?? controller.encoder { row("Encoder", encoder) }
            if let speed = status.recentSpeed ?? status.speed {
                row("Encode speed", String(format: "%.2f×", speed))
            }
            if let ahead = status.aheadSeconds {
                row("Server ahead", "\(max(0, ahead)) s\((status.suspended ?? false) ? " · held" : "")")
            }
            if let delivered = status.deliveredBps { row("Delivery rate", bitRate(delivered)) }
            if let bytes = status.deliveredBytes { row("Delivered", byteCount(bytes)) }
        }
        if let subtitle = controller.selectedSubtitle,
           let track = controller.subtitles.first(where: { $0.index == subtitle }) {
            row("Subtitles", (track.title ?? track.language?.uppercased() ?? "Track \(subtitle + 1)") + " · burned in")
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Text(label)
                .foregroundColor(Palette.muted)
                .frame(width: 100, alignment: .leading)
            Text(value)
                .foregroundColor(.white)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.system(.caption, design: .monospaced))
    }

    private func bitRate(_ bits: Int) -> String {
        String(format: "%.2f Mb/s", Double(bits) / 1_000_000.0)
    }

    private func byteCount(_ bytes: Int) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}
