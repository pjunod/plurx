import SwiftUI

#if os(tvOS)
private enum PlayerControl: Hashable {
    case reveal
    case close
    case progress
    case marker
    case skipBack
    case playPause
    case skipForward
    case pictureInPicture
    case audio
    case subtitles
    case quality
    case autoplay
    case stats
}
#endif

#if os(iOS)
private enum PlayerOptionMenu: Hashable {
    case audio
    case subtitles
    case quality
    case more
}

private struct PlayerOptionMenuButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .padding(.vertical, 9)
            .background(
                configuration.isPressed ? Color.primary.opacity(0.12) : Color.clear,
                in: RoundedRectangle(cornerRadius: 7)
            )
            .contentShape(Rectangle())
    }
}
#endif

enum PlayerSeekDirection {
    case left
    case right
    case up
    case down

    var seconds: Double {
        switch self {
        case .left: return -10
        case .right: return 10
        case .up: return 30
        case .down: return -30
        }
    }
}

struct PlayerMetadataBadge: Equatable, Identifiable {
    enum Kind: String, Equatable {
        case resolution
        case dynamicRange
        case audio
    }

    /// Mirrors the web player's `.res`, `.hdr`, `.dv`, and `.audio` badge
    /// classes. Keeping this semantic (instead of storing a `Color`) makes the
    /// badge contract testable and lets resolution continue following the
    /// viewer's selected accent palette.
    enum Tone: String, Equatable {
        case resolution
        case hdr
        case dolbyVision
        case audio
    }

    let kind: Kind
    let tone: Tone
    /// The source grade (or the whole terse label for non-HDR badges).
    let mark: String?
    let accessibilityLabel: String
    /// The grade actually rendering when it differs from `mark`. Keeping this
    /// separate lets the badge dim the unavailable source capability while
    /// leaving the functioning result (`→ HDR10`) fully lit, like the web UI.
    var renderedMark: String? = nil
    /// Only the icon/source half is subdued when `renderedMark` is present.
    var dimmed: Bool = false

    var displayMark: String? {
        guard let renderedMark else { return mark }
        return [mark, "→ \(renderedMark)"].compactMap { $0 }.joined(separator: " ")
    }

    /// Detail-page metadata still uses the native SF Symbol row. Playback uses
    /// `PlayerMetadataBadgeIcon` instead, so this compatibility mapping does
    /// not leak the old glyphs back into the player.
    var symbol: String {
        switch kind {
        case .resolution: return "tv.fill"
        case .dynamicRange: return "sparkles"
        case .audio: return "waveform"
        }
    }

    var id: String { kind.rawValue }
}

enum PlayerMetadataBadgeMetrics {
    static let rowSpacing: CGFloat = 5
    static let contentSpacing: CGFloat = 4
    static let horizontalPadding: CGFloat = 6
    static let verticalPadding: CGFloat = 2
    static let strokeWidth: CGFloat = 1
    /// Readable, but plainly the "off" treatment next to a lit chip.
    static let dimmedOpacity: Double = 0.45

    #if os(tvOS)
    static let fontSize: CGFloat = 16
    static let iconSize: CGFloat = 19
    static let tracking: CGFloat = 0.55
    #else
    static let fontSize: CGFloat = 10
    static let iconSize: CGFloat = 13
    static let tracking: CGFloat = 0.35
    #endif
}

private extension PlayerMetadataBadge.Tone {
    var color: Color {
        switch self {
        case .resolution: return Palette.accent
        case .hdr: return Color(red: 0x12 / 255, green: 0xB3 / 255, blue: 0xA6 / 255)
        case .dolbyVision: return Color(red: 0xC9 / 255, green: 0x9A / 255, blue: 0x2B / 255)
        case .audio: return Color(red: 0x7F / 255, green: 0x8F / 255, blue: 0xE0 / 255)
        }
    }

    var fillOpacity: Double {
        switch self {
        case .resolution: return 0.13
        case .hdr: return 0.14
        case .dolbyVision: return 0.15
        case .audio: return 0.14
        }
    }

    var borderOpacity: Double {
        switch self {
        case .resolution: return 0.42
        case .hdr: return 0.48
        case .dolbyVision: return 0.52
        case .audio: return 0.46
        }
    }
}

/// The exact self-hosted Material glyphs used by the web player. SwiftUI's SF
/// Symbols are excellent platform icons, but they made the same media facts
/// look unrelated across plurx clients.
private struct PlayerMetadataBadgeIcon: Shape {
    let kind: PlayerMetadataBadge.Kind

    func path(in rect: CGRect) -> Path {
        var path = Path()
        switch kind {
        case .resolution:
            path.move(to: CGPoint(x: 21, y: 3))
            path.addLine(to: CGPoint(x: 3, y: 3))
            path.addCurve(
                to: CGPoint(x: 1, y: 5),
                control1: CGPoint(x: 1.9, y: 3),
                control2: CGPoint(x: 1, y: 3.9)
            )
            path.addLine(to: CGPoint(x: 1, y: 17))
            path.addCurve(
                to: CGPoint(x: 3, y: 19),
                control1: CGPoint(x: 1, y: 18.1),
                control2: CGPoint(x: 1.9, y: 19)
            )
            path.addLine(to: CGPoint(x: 8, y: 19))
            path.addLine(to: CGPoint(x: 8, y: 21))
            path.addLine(to: CGPoint(x: 16, y: 21))
            path.addLine(to: CGPoint(x: 16, y: 19))
            path.addLine(to: CGPoint(x: 21, y: 19))
            path.addCurve(
                to: CGPoint(x: 23, y: 17),
                control1: CGPoint(x: 22.1, y: 19),
                control2: CGPoint(x: 23, y: 18.1)
            )
            path.addLine(to: CGPoint(x: 23, y: 5))
            path.addCurve(
                to: CGPoint(x: 21, y: 3),
                control1: CGPoint(x: 23, y: 3.9),
                control2: CGPoint(x: 22.1, y: 3)
            )
            path.closeSubpath()
            path.move(to: CGPoint(x: 21, y: 17))
            path.addLine(to: CGPoint(x: 3, y: 17))
            path.addLine(to: CGPoint(x: 3, y: 5))
            path.addLine(to: CGPoint(x: 21, y: 5))
            path.closeSubpath()
        case .dynamicRange:
            addPolygon([
                (19, 9), (20.25, 6.25), (23, 5), (20.25, 3.75),
                (19, 1), (17.75, 3.75), (15, 5), (17.75, 6.25)
            ], to: &path)
            addPolygon([
                (11.5, 9.5), (9, 4), (6.5, 9.5), (1, 12),
                (6.5, 14.5), (9, 20), (11.5, 14.5), (17, 12)
            ], to: &path)
            addPolygon([
                (19, 15), (17.75, 17.75), (15, 19), (17.75, 20.25),
                (19, 23), (20.25, 20.25), (23, 19), (20.25, 17.75)
            ], to: &path)
        case .audio:
            path.addRect(CGRect(x: 3, y: 10, width: 2, height: 4))
            path.addRect(CGRect(x: 7, y: 6, width: 2, height: 12))
            path.addRect(CGRect(x: 11, y: 2, width: 2, height: 20))
            path.addRect(CGRect(x: 15, y: 6, width: 2, height: 12))
            path.addRect(CGRect(x: 19, y: 10, width: 2, height: 4))
        }

        return path.applying(CGAffineTransform(
            a: rect.width / 24,
            b: 0,
            c: 0,
            d: rect.height / 24,
            tx: rect.minX,
            ty: rect.minY
        ))
    }

    private func addPolygon(
        _ points: [(CGFloat, CGFloat)],
        to path: inout Path
    ) {
        guard let first = points.first else { return }
        path.move(to: CGPoint(x: first.0, y: first.1))
        for point in points.dropFirst() {
            path.addLine(to: CGPoint(x: point.0, y: point.1))
        }
        path.closeSubpath()
    }
}

private struct PlayerMetadataBadgeView: View {
    let badge: PlayerMetadataBadge

    var body: some View {
        HStack(spacing: PlayerMetadataBadgeMetrics.contentSpacing) {
            HStack(spacing: PlayerMetadataBadgeMetrics.contentSpacing) {
                PlayerMetadataBadgeIcon(kind: badge.kind)
                    .fill(badge.tone.color, style: FillStyle(eoFill: true))
                    .frame(
                        width: PlayerMetadataBadgeMetrics.iconSize,
                        height: PlayerMetadataBadgeMetrics.iconSize
                    )
                if let mark = badge.mark {
                    Text(mark)
                }
            }
            .opacity(badge.dimmed ? PlayerMetadataBadgeMetrics.dimmedOpacity : 1)
            if let renderedMark = badge.renderedMark {
                Text("→ \(renderedMark)")
            }
        }
        .font(.system(
            size: PlayerMetadataBadgeMetrics.fontSize,
            weight: .bold,
            design: .rounded
        ))
        .tracking(PlayerMetadataBadgeMetrics.tracking)
        .foregroundStyle(badge.tone.color)
        .padding(.horizontal, PlayerMetadataBadgeMetrics.horizontalPadding)
        .padding(.vertical, PlayerMetadataBadgeMetrics.verticalPadding)
        .background {
            if !badge.dimmed {
                Capsule().fill(badge.tone.color.opacity(badge.tone.fillOpacity))
            }
        }
        .overlay {
            Capsule().stroke(
                badge.tone.color.opacity(badge.tone.borderOpacity),
                lineWidth: PlayerMetadataBadgeMetrics.strokeWidth
            )
        }
        .fixedSize(horizontal: true, vertical: false)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(badge.accessibilityLabel)
    }
}

struct PlayerOverlayVisibility: Equatable {
    let controls: Bool
    let playbackInfo: Bool
}

#if os(iOS)
/// The landscape/iPad transport keeps its fixed control groups at the edges
/// and gives every remaining point to the timeline. Keeping this as a real
/// layout boundary prevents a `fixedSize` measurement from becoming the
/// selected row's final width inside `ViewThatFits`.
struct PlayerTouchWideRow<Transport: View, Timeline: View, Options: View>: View {
    let transport: Transport
    let timeline: Timeline
    let options: Options

    init(
        @ViewBuilder transport: () -> Transport,
        @ViewBuilder timeline: () -> Timeline,
        @ViewBuilder options: () -> Options
    ) {
        self.transport = transport()
        self.timeline = timeline()
        self.options = options()
    }

    var body: some View {
        HStack(spacing: 8) {
            transport
            timeline
                .layoutPriority(1)
            options
        }
        .frame(maxWidth: .infinity)
    }
}
#endif

/// Source vs delivered vs rendered, for the one badge that has to answer both
/// "what is this file?" and "what am I getting?" — MEDIA-BADGES-PLAN.md §2.
///
/// A reporter and nothing else. No value computed here reaches a decision, a
/// capability query, or a session request; the badge changed, the pipeline did
/// not (that plan's §9).
enum DynamicRange {
    static let dolbyVision = "dolby_vision"
    static let hdr10 = "hdr10"
    static let hlg = "hlg"
    static let sdr = "sdr"

    /// The coarse grade of a source in the server's own vocabulary, so a source
    /// and `delivered_dynamic_range` compare by string equality. Both fields
    /// are read because a file can carry only the rich `hdr_format` label.
    static func source(hdr: String?, hdrFormat: String?) -> String? {
        let label = hdrFormat?.trimmingCharacters(in: .whitespaces) ?? ""
        let coarse = hdr?.trimmingCharacters(in: .whitespaces).lowercased() ?? ""
        if coarse == dolbyVision || label.localizedCaseInsensitiveContains("dolby") {
            return dolbyVision
        }
        if coarse == sdr { return nil }
        if !coarse.isEmpty { return coarse }
        if label.isEmpty { return nil }
        // "HDR10+", "HDR10", "SMPTE ST 2084" — anything left that names a grade
        // without saying HLG is a PQ grade.
        return label.localizedCaseInsensitiveContains("hlg") ? hlg : hdr10
    }

    /// The terse chip text used by the web player. Dolby Vision retains its
    /// probed profile (`DV P8`); other HDR formats use the already-short rich
    /// source string when one is available.
    static func sourceMark(_ grade: String, hdrFormat: String?) -> String {
        let rich = hdrFormat?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if grade == dolbyVision {
            if let profile = dolbyVisionProfile(in: rich) {
                return "DV P\(profile)"
            }
            return "DV"
        }
        return rich.isEmpty ? shortLabel(grade) : rich.uppercased()
    }

    static func sourceLabel(_ grade: String, hdrFormat: String?) -> String {
        let rich = hdrFormat?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return rich.isEmpty ? longLabel(grade) : rich
    }

    private static func dolbyVisionProfile(in label: String) -> String? {
        guard let range = label.range(
            of: #"profile\s*[0-9]+"#,
            options: [.regularExpression, .caseInsensitive]
        ) else { return nil }
        let digits = label[range].filter { $0.isNumber }
        return digits.isEmpty ? nil : String(digits)
    }

    static func shortLabel(_ grade: String) -> String {
        switch grade {
        case Self.dolbyVision: return "DV"
        case Self.hdr10: return "HDR10"
        case Self.hlg: return "HLG"
        case Self.sdr: return "SDR"
        default: return grade.uppercased()
        }
    }

    static func longLabel(_ grade: String) -> String {
        switch grade {
        case Self.dolbyVision: return "Dolby Vision"
        case Self.hdr10: return "HDR10"
        case Self.hlg: return "HLG"
        case Self.sdr: return "SDR"
        default: return grade.uppercased()
        }
    }

    /// What is on the panel. Delivered bits are necessary but not sufficient:
    /// an HDR10 direct play on an SDR display is delivered HDR and rendered
    /// SDR. `AVPlayer.eligibleForHDRPlayback` (via `Caps.displayIsHDR`) is the
    /// documented signal and the whole of it — there is no public per-variant
    /// introspection for HLS, and headroom polling is a stated non-goal.
    static func rendered(delivered: String, displayHDR: Bool) -> String {
        (delivered != sdr && !displayHDR) ? sdr : delivered
    }

    /// The server already says *why* ("Dolby Vision metadata removed for this
    /// device; compatible HDR base kept") — better than anything invented here.
    /// Only borrow a reason that is actually about the picture.
    static func reason(from reasons: [String]?) -> String? {
        reasons?.first {
            $0.localizedCaseInsensitiveContains("dolby vision")
                || $0.localizedCaseInsensitiveContains("hdr")
                || $0.localizedCaseInsensitiveContains("tone")
        }
    }
}

/// Full-screen Apple player with an explicit on-demand transport. AVPlayer
/// sees a growing plurx HLS playlist as an EVENT stream while the server is
/// producing it, so relying on the system overlay alone labels a movie LIVE
/// and can replace Pause with Stop. The controls here use the known film
/// runtime and work for direct, remux, and transcode delivery alike.
struct PlayerView: View {
    static let controlAutoHideDelayNanoseconds: UInt64 = 4_000_000_000

    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    let itemId: Int
    let fileId: Int
    let startMs: Int
    let durationMs: Int
    let title: String
    var subtitle: String? = nil
    var year: Int? = nil
    var airDate: String? = nil
    var overview: String? = nil
    var offlineItem: OfflineItem? = nil
    var onPlayNext: ((PlayContext) -> Void)?
    /// Hands the owning detail screen the last on-screen position immediately.
    /// The server progress write is intentionally best-effort and asynchronous;
    /// without this handoff the still-present detail view keeps rendering the
    /// resume point it loaded before playback began.
    var onPlaybackStopped: ((Int) -> Void)?

    @StateObject private var controller = PlayerController()
    @StateObject private var pictureInPicture = PictureInPictureController()
    @State private var showStats = false
    @State private var findingNext = false
    @State private var isScrubbing = false
    @State private var scrubMs = 0.0
    @State private var controlsVisible = true
    @State private var autoHideGeneration = 0
    #if os(iOS)
    @State private var activeOptionMenu: PlayerOptionMenu?
    #endif
    #if os(tvOS)
    @FocusState private var focusedControl: PlayerControl?
    #endif

    var body: some View {
        ZStack(alignment: .topLeading) {
            Color.black.ignoresSafeArea()

            PlayerSurface(
                player: controller.player,
                pictureInPicture: pictureInPicture,
                pgsOverlay: controller.pgsOverlayWindow
            )
                .ignoresSafeArea()

            #if os(tvOS)
            if !controlsVisible {
                Color.clear
                    .contentShape(Rectangle())
                    .ignoresSafeArea()
                    .focusable()
                    .focusEffectDisabled()
                    .focused($focusedControl, equals: .reveal)
                    .onTapGesture { revealControlsFromRemote() }
                    .onMoveCommand { direction in seekFromRemote(direction) }
                    .onPlayPauseCommand {
                        controller.togglePlayPause()
                        revealControlsFromRemote()
                    }
                    .accessibilityLabel("Show playback controls")
            }
            #endif

            #if os(iOS)
            Color.clear
                .contentShape(Rectangle())
                .ignoresSafeArea()
                .onTapGesture { toggleControls() }
                .accessibilityLabel("Show or hide playback controls")
            #endif

            if controller.failed {
                failureView
            } else {
                if overlayVisibility.controls {
                    VStack(spacing: 0) {
                        HStack(alignment: .top) {
                            #if os(iOS)
                            closeButton
                            #endif
                            Spacer()
                        }
                        Spacer()
                        playbackControls
                    }
                    .padding(20)
                    .transition(.opacity)
                }

                if overlayVisibility.playbackInfo {
                    PlaybackStatsView(
                        controller: controller,
                        onDismiss: dismissPlaybackInfo
                    )
                    .frame(maxWidth: .infinity, alignment: .trailing)
                    .padding(20)
                    .transition(.opacity.combined(with: .move(edge: .trailing)))
                }
            }

            if controller.isChangingStream {
                streamChangeProgress
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

            if let error = controller.playbackNotice
                ?? controller.playbackError
                ?? pictureInPicture.errorMessage,
               !controller.failed {
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
            #if os(iOS)
            if let offlineItem {
                controller.startOffline(model: model, item: offlineItem)
            } else {
                controller.start(
                    model: model,
                    itemId: itemId,
                    fileId: fileId,
                    startMs: startMs,
                    durationMs: durationMs,
                    title: title
                )
            }
            #else
            controller.start(
                model: model,
                itemId: itemId,
                fileId: fileId,
                startMs: startMs,
                durationMs: durationMs,
                title: title
            )
            #endif
            #if os(tvOS)
            try? await Task.sleep(nanoseconds: 100_000_000)
            focusedControl = .playPause
            #endif
        }
        .onDisappear {
            let stoppedAt = controller.realPositionMs()
            pictureInPicture.stop()
            controller.stop()
            onPlaybackStopped?(stoppedAt)
        }
        .task(id: autoHideGeneration) {
            guard Self.shouldAutoHideControls(
                visible: controlsVisible,
                scrubbing: isScrubbing,
                changingStream: controller.isChangingStream,
                optionMenuOpen: optionMenuOpen
            ) else { return }
            try? await Task.sleep(nanoseconds: Self.controlAutoHideDelayNanoseconds)
            guard !Task.isCancelled,
                  Self.shouldAutoHideControls(
                      visible: controlsVisible,
                      scrubbing: isScrubbing,
                      changingStream: controller.isChangingStream,
                      optionMenuOpen: optionMenuOpen
                  ) else { return }
            hideControls()
        }
        .onChange(of: controller.isPlaying) { _, _ in revealControls() }
        .onChange(of: controller.isChangingStream) { _, _ in revealControls() }
        .onChange(of: showStats) { _, _ in restartAutoHideTimer() }
        .onChange(of: isScrubbing) { _, _ in restartAutoHideTimer() }
        .onChange(of: optionMenuOpen) { _, _ in restartAutoHideTimer() }
        .onChange(of: controller.finished) { _, finished in
            guard finished, model.autoplay, offlineItem == nil else { return }
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
        .onChange(of: focusedControl) { _, _ in
            if controlsVisible { restartAutoHideTimer() }
        }
        .onExitCommand {
            if showStats {
                dismissPlaybackInfo()
            } else if controlsVisible {
                hideControls()
            } else {
                dismiss()
            }
        }
        #endif
    }

    private var overlayVisibility: PlayerOverlayVisibility {
        Self.overlayVisibility(
            controlsVisible: controlsVisible,
            playbackInfoVisible: showStats
        )
    }

    static func overlayVisibility(
        controlsVisible: Bool,
        playbackInfoVisible: Bool
    ) -> PlayerOverlayVisibility {
        PlayerOverlayVisibility(
            controls: controlsVisible,
            playbackInfo: playbackInfoVisible
        )
    }

    static func shouldAutoHideControls(
        visible: Bool,
        scrubbing: Bool,
        changingStream: Bool,
        optionMenuOpen: Bool
    ) -> Bool {
        visible && !scrubbing && !changingStream && !optionMenuOpen
    }

    /// A presented touch menu is hosted outside the control hierarchy. Removing
    /// the controls therefore removes its presentation anchor and dismisses the
    /// menu too. On tvOS, focus on a menu button is the equivalent interaction:
    /// keep the chrome up while the viewer is opening or navigating that menu.
    private var optionMenuOpen: Bool {
        #if os(iOS)
        activeOptionMenu != nil
        #else
        switch focusedControl {
        case .audio, .subtitles, .quality: true
        default: false
        }
        #endif
    }

    private func toggleControls() {
        #if os(iOS)
        withAnimation(.easeInOut(duration: 0.2)) {
            controlsVisible.toggle()
        }
        restartAutoHideTimer()
        #endif
    }

    private func revealControls() {
        withAnimation(.easeInOut(duration: 0.2)) { controlsVisible = true }
        restartAutoHideTimer()
    }

    private func restartAutoHideTimer() {
        autoHideGeneration &+= 1
    }

    private func hideControls() {
        guard controlsVisible else { return }
        #if os(iOS)
        activeOptionMenu = nil
        #endif
        withAnimation(.easeOut(duration: 0.2)) {
            controlsVisible = false
        }
        #if os(tvOS)
        focusedControl = nil
        Task { @MainActor in
            await Task.yield()
            if !controlsVisible { focusedControl = .reveal }
        }
        #endif
    }

    private func dismissPlaybackInfo() {
        withAnimation { showStats = false }
        #if os(tvOS)
        revealControlsFromRemote()
        #else
        revealControls()
        #endif
    }

    #if os(tvOS)
    private func revealControlsFromRemote() {
        revealControls()
        Task { @MainActor in
            await Task.yield()
            if controlsVisible { focusedControl = .playPause }
        }
    }

    private func seekFromRemote(_ direction: MoveCommandDirection) {
        let seekDirection: PlayerSeekDirection
        switch direction {
        case .left: seekDirection = .left
        case .right: seekDirection = .right
        case .up: seekDirection = .up
        case .down: seekDirection = .down
        @unknown default: return
        }
        controller.skip(seconds: seekDirection.seconds)
    }
    #endif

    @ViewBuilder
    private var streamChangeProgress: some View {
        #if os(iOS)
        ProgressView().controlSize(.large)
        #else
        ProgressView()
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
        VStack(alignment: .leading, spacing: 8) {
            playbackInfoHeader

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
            HStack(spacing: 12) {
                transportControlGroup
                if controller.knownDurationMs > 0 {
                    playbackTimeLabel(controller.currentMs)
                    tvProgressBar
                        .layoutPriority(1)
                    playbackTimeLabel(controller.knownDurationMs)
                }
                playbackOptionGroup
            }
            #else
            touchPlaybackRows
            #endif
        }
        #if os(tvOS)
        .font(.body)
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .foregroundStyle(.white)
        .frame(maxWidth: .infinity)
        #else
        .font(.body)
        .buttonStyle(IOSPlayerControlButtonStyle())
        .foregroundStyle(.white)
        .frame(maxWidth: .infinity)
        #endif
    }

    static func nowPlayingSummary(_ overview: String?) -> String {
        let summary = overview?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return summary.isEmpty ? "No description available." : summary
    }

    static func playbackDateLabel(airDate: String?, year: Int?) -> String? {
        let raw = airDate?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if !raw.isEmpty {
            let parser = DateFormatter()
            parser.calendar = Calendar(identifier: .gregorian)
            parser.locale = Locale(identifier: "en_US_POSIX")
            parser.timeZone = TimeZone(secondsFromGMT: 0)
            parser.dateFormat = "yyyy-MM-dd"
            if let date = parser.date(from: String(raw.prefix(10))) {
                let display = DateFormatter()
                display.calendar = parser.calendar
                display.locale = Locale(identifier: "en_US_POSIX")
                display.timeZone = parser.timeZone
                display.setLocalizedDateFormatFromTemplate("MMM d, yyyy")
                return display.string(from: date)
            }
            return raw
        }
        return year.map(String.init)
    }

    private func playbackTimeLabel(_ milliseconds: Int) -> some View {
        Text(formatTime(milliseconds))
            .fixedSize()
            #if os(tvOS)
            .padding(.horizontal, TVPlayerChromeMetrics.timeHorizontalInset)
            .padding(.vertical, TVPlayerChromeMetrics.timeVerticalInset)
            .background(.black.opacity(0.58), in: Capsule())
            #endif
    }

    private var playbackInfoHeader: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(playbackHeading)
                #if os(tvOS)
                .font(.system(size: 30, weight: .bold))
                #else
                .font(.system(size: 26, weight: .bold))
                #endif
                .foregroundColor(.white)
                .lineLimit(1)

            playbackFacts

            if !playbackContext.isEmpty {
                Text(playbackContext.joined(separator: "   ·   "))
                #if os(tvOS)
                .font(.system(size: 21, weight: .medium, design: .rounded))
                #else
                .font(.system(size: 15, weight: .medium, design: .rounded))
                #endif
                .foregroundColor(.white.opacity(0.7))
                .lineLimit(1)
            }

            Text(Self.nowPlayingSummary(overview))
                #if os(tvOS)
                .font(.system(size: TVPlayerChromeMetrics.infoBodyFontSize, weight: .regular))
                .lineLimit(3)
                #else
                .font(.system(size: 15, weight: .regular))
                .lineLimit(3)
                #endif
                .foregroundStyle(.white.opacity(0.88))
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: 1_180, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var playbackHeading: String {
        [subtitle, title]
            .compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
            .joined(separator: "   ·   ")
    }

    private var playbackContext: [String] {
        [Self.playbackDateLabel(airDate: airDate, year: year), runtimeLabel]
            .compactMap { $0 }
            .filter { !$0.isEmpty }
    }

    private var playbackFacts: some View {
        let audio = controller.audioTracks.first { $0.index == controller.selectedAudio }
            ?? controller.audioTracks.first { $0.default }
            ?? controller.audioTracks.first
        // Eligibility is asked here, at render time, rather than cached at
        // launch: an Apple TV whose Dolby Vision output is turned off in
        // Settings changes this answer while the app is running.
        let badges = Self.playbackBadges(
            source: controller.decision?.source,
            audio: audio,
            delivered: controller.deliveredRange,
            displayHDR: Caps.displayIsHDR
        )
        return HStack(spacing: PlayerMetadataBadgeMetrics.rowSpacing) {
            ForEach(badges) { badge in
                PlayerMetadataBadgeView(badge: badge)
            }
        }
    }

    private var runtimeLabel: String? {
        let milliseconds = controller.knownDurationMs > 0
            ? controller.knownDurationMs
            : durationMs
        guard milliseconds > 0 else { return nil }
        let totalMinutes = milliseconds / 60_000
        let hours = totalMinutes / 60
        let minutes = totalMinutes % 60
        return hours > 0 ? "\(hours)h \(minutes)m" : "\(minutes)m"
    }

    /// `delivered`/`displayHDR` default to "no session yet, assume nothing is
    /// being lost", which is exactly the source-only state the detail screens
    /// and an older server both want.
    static func playbackBadges(
        source: SourceSummary?,
        audio: AudioTrack?,
        delivered: String? = nil,
        displayHDR: Bool = true
    ) -> [PlayerMetadataBadge] {
        var badges: [PlayerMetadataBadge] = []
        if let label = playbackResolutionLabel(width: source?.width, height: source?.height) {
            badges.append(PlayerMetadataBadge(
                kind: .resolution,
                tone: .resolution,
                mark: label.uppercased(),
                accessibilityLabel: label
            ))
        }
        if let range = dynamicRangeBadge(
            hdr: source?.hdr,
            hdrFormat: source?.hdrFormat,
            delivered: delivered,
            displayHDR: displayHDR
        ) {
            badges.append(range)
        }
        if let audio, let sound = soundLabel(audio) {
            badges.append(PlayerMetadataBadge(
                kind: .audio,
                tone: .audio,
                mark: sound.mark,
                accessibilityLabel: sound.accessibilityLabel
            ))
        }
        return badges
    }

    /// The three states of MEDIA-BADGES-PLAN §2.3, as a pure function of
    /// (source grade, delivered grade, display capability):
    ///
    /// - **lit** — rendered grade equals the source grade: today's chip.
    /// - **different grade** — they differ: the source half dims and the arrow
    ///   suffix names what is actually on screen (`DV → HDR10`) at full
    ///   brightness, matching the web player's capability/function split.
    /// - **source-only** — `delivered` is nil (no session, or a server that
    ///   does not report it): today's chip, unchanged.
    ///
    /// The badge text always starts from what the file carries, because that
    /// claim stays true either way; what changes is whether it is being kept.
    static func dynamicRangeBadge(
        hdr: String?,
        hdrFormat: String?,
        delivered: String?,
        displayHDR: Bool
    ) -> PlayerMetadataBadge? {
        guard let source = DynamicRange.source(hdr: hdr, hdrFormat: hdrFormat) else {
            return nil
        }
        let sourceMark = DynamicRange.sourceMark(source, hdrFormat: hdrFormat)
        let sourceLabel = DynamicRange.sourceLabel(source, hdrFormat: hdrFormat)
        let tone: PlayerMetadataBadge.Tone = source == DynamicRange.dolbyVision
            ? .dolbyVision
            : .hdr
        let lit = PlayerMetadataBadge(
            kind: .dynamicRange,
            tone: tone,
            mark: sourceMark,
            accessibilityLabel: sourceLabel
        )
        guard let delivered, !delivered.isEmpty else { return lit }
        let rendered = DynamicRange.rendered(delivered: delivered, displayHDR: displayHDR)
        guard rendered != source else { return lit }
        return PlayerMetadataBadge(
            kind: .dynamicRange,
            tone: tone,
            mark: sourceMark,
            accessibilityLabel:
                "\(DynamicRange.longLabel(source)), playing as \(DynamicRange.longLabel(rendered))",
            renderedMark: DynamicRange.shortLabel(rendered),
            dimmed: true
        )
    }

    /// Exact port of the web player's `resLabel`: it uses both raster edges so
    /// portrait metadata and scope-cropped masters stay in the intended tier.
    static func playbackResolutionLabel(width: Int?, height: Int?) -> String? {
        let validWidth = (width ?? 0) > 0 ? width ?? 0 : 0
        let validHeight = (height ?? 0) > 0 ? height ?? 0 : 0
        let shortEdge: Int
        let longEdge: Int
        if validWidth > 0, validHeight > 0 {
            shortEdge = min(validWidth, validHeight)
            longEdge = max(validWidth, validHeight)
        } else {
            shortEdge = validHeight > 0 ? validHeight : validWidth
            longEdge = shortEdge
        }
        guard shortEdge > 0 else { return nil }
        if longEdge >= 3_200 || shortEdge >= 1_700 { return "2160p" }
        if longEdge >= 2_300 || shortEdge >= 1_300 { return "1440p" }
        if longEdge >= 1_600 || shortEdge >= 900 { return "1080p" }
        if longEdge >= 1_100 || shortEdge >= 650 { return "720p" }
        if longEdge >= 700 || shortEdge >= 400 { return "480p" }
        return "\(shortEdge)p"
    }

    /// The same three-layer truth in one sentence, for the playback-info
    /// panel's "Dynamic range" row — the web player's equivalent row worded the
    /// same way. Nil when there is nothing to report: an SDR source with no
    /// session on it.
    static func dynamicRangeSummary(
        source: SourceSummary?,
        delivered: String?,
        displayHDR: Bool,
        reasons: [String]?
    ) -> String? {
        let grade = DynamicRange.source(hdr: source?.hdr, hdrFormat: source?.hdrFormat)
            ?? DynamicRange.sdr
        guard let delivered, !delivered.isEmpty else {
            guard grade != DynamicRange.sdr else { return nil }
            // No session and no report: the rich source label is all there is.
            return source?.hdrFormat ?? DynamicRange.longLabel(grade)
        }
        let rendered = DynamicRange.rendered(delivered: delivered, displayHDR: displayHDR)
        let long = DynamicRange.longLabel(rendered)
        guard rendered != grade else { return "\(long) (rendering)" }
        let displayLoss = delivered != DynamicRange.sdr && !displayHDR
        let note: String
        if displayLoss {
            note = "this display is not HDR"
        } else if let served = DynamicRange.reason(from: reasons) {
            note = served
        } else if rendered == DynamicRange.sdr {
            note = "tone-mapped from \(DynamicRange.longLabel(grade))"
        } else {
            note = "delivered as \(long)"
        }
        return "\(long) — \(note)"
    }

    static func playbackFacts(
        source: SourceSummary?,
        audio: AudioTrack?,
        delivered: String? = nil,
        displayHDR: Bool = true
    ) -> [String] {
        playbackBadges(
            source: source,
            audio: audio,
            delivered: delivered,
            displayHDR: displayHDR
        ).map(\.accessibilityLabel)
    }

    static func soundLabel(_ track: AudioTrack) -> (mark: String, accessibilityLabel: String)? {
        let title = track.title ?? ""
        let format: (mark: String, label: String)
        if title.localizedCaseInsensitiveContains("atmos") {
            format = ("ATMOS", "Dolby Atmos")
        } else {
            switch track.codec.lowercased() {
            case "eac3", "e-ac-3": format = ("DD+", "Dolby Digital Plus")
            case "ac3", "ac-3": format = ("DD", "Dolby Digital")
            case "truehd": format = ("TRUEHD", "Dolby TrueHD")
            case "dts", "dca": format = ("DTS", "DTS")
            case "aac": format = ("AAC", "AAC")
            case "flac": format = ("FLAC", "FLAC")
            case "opus": format = ("OPUS", "Opus")
            default:
                let codec = track.codec.uppercased()
                format = (codec, codec)
            }
        }
        let channels: (mark: String, label: String)?
        switch track.channels {
        case 1: channels = ("MONO", "Mono")
        case 2: channels = ("2.0", "2.0")
        case 6: channels = ("5.1", "5.1")
        case 7: channels = ("6.1", "6.1")
        case 8: channels = ("7.1", "7.1")
        case let count?: channels = ("\(count)CH", "\(count)ch")
        case nil: channels = nil
        }
        guard !format.mark.isEmpty else { return nil }
        return (
            [format.mark, channels?.mark].compactMap { $0 }.joined(separator: " "),
            [format.label, channels?.label].compactMap { $0 }.joined(separator: " ")
        )
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
            transportControlGroup
            Spacer(minLength: 8)
            playbackOptionGroup
        }
    }

    #if os(iOS)
    /// Uses the Apple TV's single-row hierarchy whenever the touch viewport is
    /// wide enough (iPhone landscape and iPad), with the same hierarchy split
    /// over two rows only when a portrait phone cannot fit it safely.
    private var touchPlaybackRows: some View {
        ViewThatFits(in: .horizontal) {
            PlayerTouchWideRow {
                transportControlGroup
            } timeline: {
                if controller.knownDurationMs > 0 {
                    playbackTimeLabel(Int(isScrubbing ? scrubMs : Double(controller.currentMs)))
                    touchProgressSlider
                        .frame(minWidth: 100)
                    playbackTimeLabel(controller.knownDurationMs)
                }
            } options: {
                playbackOptionGroup
            }

            VStack(alignment: .leading, spacing: 8) {
                if controller.knownDurationMs > 0 {
                    HStack(spacing: 10) {
                        playbackTimeLabel(Int(isScrubbing ? scrubMs : Double(controller.currentMs)))
                        touchProgressSlider
                        playbackTimeLabel(controller.knownDurationMs)
                    }
                }
                ViewThatFits(in: .horizontal) {
                    expandedControlRow
                        .fixedSize(horizontal: true, vertical: false)
                    compactControlRow
                }
                .frame(maxWidth: .infinity)
            }
        }
        .font(.system(.caption, design: .monospaced))
        .foregroundColor(.white)
    }

    private var touchProgressSlider: some View {
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
    }
    #endif

    private var transportControlGroup: some View {
        HStack(spacing: 8) {
            skipBackButton
            playPauseButton
            skipForwardButton
        }
    }

    private var playbackOptionGroup: some View {
        HStack(spacing: 8) {
            if pictureInPicture.isSupported { pictureInPictureButton }
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
            if pictureInPicture.isSupported { pictureInPictureButton }
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .skipForward)
        #endif
    }

    private var pictureInPictureButton: some View {
        Button {
            guard controller.allowsPictureInPictureCommand() else { return }
            pictureInPicture.toggle()
            revealControls()
        } label: {
            Image(systemName: pictureInPicture.isActive ? "pip.exit" : "pip.enter")
                .foregroundStyle(pictureInPicture.isActive ? Palette.accent : .white)
        }
        .disabled(!pictureInPicture.isActive && !pictureInPicture.isPossible)
        .accessibilityLabel(pictureInPicture.isActive
                            ? "Stop Picture in Picture"
                            : "Start Picture in Picture")
        #if os(tvOS)
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .pictureInPicture)
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .stats)
        #endif
    }

    #if os(iOS)
    private var moreMenu: some View {
        Button {
            presentOptionMenu(.more)
        } label: {
            Image(systemName: "ellipsis.circle.fill")
        }
        .accessibilityLabel("More playback options")
        .popover(isPresented: optionMenuBinding(.more), arrowEdge: .bottom) {
            optionMenuPanel("Playback options") {
                if controller.audioTracks.count > 1 {
                    optionMenuSection("Audio") { audioChoices }
                }
                if !controller.subtitles.isEmpty {
                    optionMenuSection("Subtitles") { subtitleChoices }
                }
                if !controller.qualityRungs.isEmpty {
                    optionMenuSection("Quality") { qualityChoices }
                }
                Divider()
                Button {
                    model.setAutoplay(!model.autoplay)
                    dismissOptionMenu()
                    revealControls()
                } label: {
                    Label(model.autoplay ? "Turn off autoplay" : "Turn on autoplay",
                          systemImage: "play.square.stack.fill")
                }
                Button {
                    withAnimation { showStats.toggle() }
                    dismissOptionMenu()
                    revealControls()
                } label: {
                    Label(showStats ? "Hide playback info" : "Playback info",
                          systemImage: "info.circle.fill")
                }
            }
        }
    }
    #endif

    #if os(tvOS)
    /// SwiftUI's Slider is unavailable on tvOS. This focusable bar uses the
    /// Siri Remote's left/right commands to move through the same absolute
    /// film timeline in 10-second steps. This is deliberately not a Button:
    /// tvOS adds a large white pressed/focus surround to Buttons even when the
    /// ordinary focus effect is disabled.
    private var tvProgressBar: some View {
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
            .overlay {
                if focusedControl == .progress {
                    ZStack {
                        Capsule().stroke(
                            .black.opacity(0.96),
                            lineWidth: TVPlayerProgressFocusRing.outerStrokeWidth
                        )
                        Capsule().stroke(
                            Palette.accent.opacity(0.34),
                            lineWidth: TVPlayerProgressFocusRing.fadeStrokeWidth
                        )
                        Capsule().stroke(
                            Palette.accent,
                            lineWidth: TVPlayerProgressFocusRing.accentStrokeWidth
                        )
                    }
                }
            }
        }
        .frame(height: 8)
        .contentShape(Rectangle())
        .focusable()
        .focusEffectDisabled()
        .focused($focusedControl, equals: .progress)
        .onMoveCommand { direction in
            switch direction {
            case .left: controller.skip(seconds: PlayerSeekDirection.left.seconds)
            case .right: controller.skip(seconds: PlayerSeekDirection.right.seconds)
            case .down: focusedControl = .playPause
            default: break
            }
            revealControls()
        }
        .accessibilityLabel("Playback position. Left or right seeks 10 seconds.")
    }
    #endif

    private var audioMenu: some View {
        #if os(iOS)
        Button {
            presentOptionMenu(.audio)
        } label: {
            Image(systemName: "speaker.wave.2.fill")
        }
        .accessibilityLabel("Audio track")
        .popover(isPresented: optionMenuBinding(.audio), arrowEdge: .bottom) {
            optionMenuPanel("Audio") { audioChoices }
        }
        #else
        Menu { audioChoices } label: {
            Image(systemName: "speaker.wave.2.fill")
        }
        .accessibilityLabel("Audio track")
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .audio)
        #endif
        }

    private var subtitleMenu: some View {
        #if os(iOS)
        Button {
            presentOptionMenu(.subtitles)
        } label: {
            Image(systemName: "captions.bubble.fill")
                .foregroundStyle(controller.selectedSubtitle == nil ? .white : Palette.accent)
        }
        .accessibilityLabel("Subtitles")
        .popover(isPresented: optionMenuBinding(.subtitles), arrowEdge: .bottom) {
            optionMenuPanel("Subtitles") { subtitleChoices }
        }
        #else
        Menu { subtitleChoices } label: {
            Image(systemName: "captions.bubble.fill")
                .foregroundStyle(controller.selectedSubtitle == nil ? .white : Palette.accent)
        }
        .accessibilityLabel("Subtitles")
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .subtitles)
        #endif
        }

    private var qualityMenu: some View {
        #if os(iOS)
        Button {
            presentOptionMenu(.quality)
        } label: {
            Image(systemName: "slider.horizontal.3")
        }
        .accessibilityLabel("Playback quality")
        .popover(isPresented: optionMenuBinding(.quality), arrowEdge: .bottom) {
            optionMenuPanel("Playback quality") { qualityChoices }
        }
        #else
        Menu { qualityChoices } label: {
            Image(systemName: "slider.horizontal.3")
        }
        .accessibilityLabel("Playback quality")
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .quality)
        #endif
        }

    #if os(iOS)
    private func optionMenuBinding(_ menu: PlayerOptionMenu) -> Binding<Bool> {
        Binding(
            get: { activeOptionMenu == menu },
            set: { presented in
                activeOptionMenu = presented ? menu : nil
            }
        )
    }

    private func presentOptionMenu(_ menu: PlayerOptionMenu) {
        activeOptionMenu = menu
        revealControls()
    }

    private func dismissOptionMenu() {
        activeOptionMenu = nil
    }

    private func optionMenuPanel<Content: View>(
        _ title: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.headline)
                    .padding(.horizontal, 10)
                    .padding(.bottom, 4)
                content()
            }
            .buttonStyle(PlayerOptionMenuButtonStyle())
            .padding(8)
        }
        .frame(minWidth: 260, idealWidth: 320, maxWidth: 380, maxHeight: 430)
        // The player chrome is always white, but a popover can use light material.
        // Re-enter the presentation's semantic palette instead of inheriting white.
        .foregroundStyle(.primary)
        .presentationCompactAdaptation(.popover)
    }

    private func optionMenuSection<Content: View>(
        _ title: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        Group {
            Text(title.uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 10)
                .padding(.top, 4)
            content()
            Divider()
        }
    }
    #endif

    @ViewBuilder
    private var audioChoices: some View {
        ForEach(controller.audioTracks) { track in
            Button {
                controller.selectAudio(track.index)
                #if os(iOS)
                dismissOptionMenu()
                #endif
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
            #if os(iOS)
            dismissOptionMenu()
            #endif
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
                #if os(iOS)
                dismissOptionMenu()
                #endif
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
            #if os(iOS)
            dismissOptionMenu()
            #endif
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
                #if os(iOS)
                dismissOptionMenu()
                #endif
                revealControls()
            } label: {
                Label(
                    "\(rung.height)p  \(rung.totalKbps / 1000) Mb/s",
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
            .joined(separator: "  ")
    }

    private func subtitleLabel(_ track: SubtitleTrack) -> String {
        var parts = [track.title, languageName(track.language), track.codec.uppercased()]
            .compactMap { $0 }
        if track.forced { parts.append("Forced") }
        if !track.isNativeHLS { parts.append("Burn-in") }
        return parts.joined(separator: "  ")
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
    let onDismiss: () -> Void

    /// This panel floats over the video, so its contrast must not follow the
    /// app's light/dark palette. A fixed dark surface keeps the white copy
    /// readable in every appearance and over both bright and dark frames.
    private let panelSurface = Palette.playerChrome.opacity(0.96)
    private let labelColor = Color.white.opacity(0.82)

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 7) {
                HStack(spacing: 10) {
                    Text("Playback info")
                        .font(.system(.headline, design: .monospaced))
                        .foregroundColor(.white)
                    Spacer()
                    #if os(iOS)
                    Button(action: onDismiss) {
                        Image(systemName: "xmark")
                    }
                    .buttonStyle(.plain)
                    .foregroundColor(.white.opacity(0.72))
                    .accessibilityLabel("Close playback info")
                    #endif
                }
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
        .background(panelSurface, in: RoundedRectangle(cornerRadius: 12))
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
        // The badge says it in three characters; this says it in words, with
        // the server's own reason for the difference.
        if let range = PlayerView.dynamicRangeSummary(
            source: controller.decision?.source,
            delivered: controller.deliveredRange,
            displayHDR: Caps.displayIsHDR,
            reasons: controller.decision?.reasons
        ) {
            row("Dynamic range", range)
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
            let delivery = track.isPGSOverlay
                ? (controller.pgsOverlayStatus.label ?? "PGS overlay")
                : (track.isNativeHLS ? "native WebVTT" : "burned in")
            row("Subtitles", (track.title ?? track.language?.uppercased() ?? "Track \(subtitle + 1)") + " · \(delivery)")
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Text(label)
                .foregroundColor(labelColor)
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
