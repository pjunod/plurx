import SwiftUI

#if os(iOS)
import UIKit
#endif

enum PlayerControl: Hashable {
    case reveal
    case close
    case retry
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

#if os(iOS)
private enum PlayerOptionMenu: Hashable {
    case audio
    case subtitles
    case quality
    case more
}

enum PlayerOptionMenuPalette {
    /// UIKit semantic colors are intentional here. `foregroundStyle(.primary)`
    /// would select the primary level of the player's inherited white style,
    /// leaving white labels on a light popover.
    static let foreground = UIColor.label
    static let secondaryForeground = UIColor.secondaryLabel
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

enum PlayerSeekDirection: CaseIterable {
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

enum PlayerRemoteMoveOutcome: Equatable {
    case seek(seconds: Double)
    case focus(PlayerControl)
    case ignore
}

enum TVPlayerRemoteRouting {
    static func moveOutcome(
        focusedControl: PlayerControl,
        progressEngaged: Bool,
        direction: PlayerSeekDirection,
        progressRightNeighbor: PlayerControl = .autoplay,
        markerAvailable: Bool = false
    ) -> PlayerRemoteMoveOutcome {
        switch focusedControl {
        case .reveal:
            return .seek(seconds: direction.seconds)
        case .progress:
            switch direction {
            case .left:
                return progressEngaged
                    ? .seek(seconds: PlayerSeekDirection.left.seconds)
                    : .focus(.skipForward)
            case .right:
                return progressEngaged
                    ? .seek(seconds: PlayerSeekDirection.right.seconds)
                    : .focus(progressRightNeighbor)
            case .down:
                return .focus(.playPause)
            case .up:
                return markerAvailable ? .focus(.marker) : .ignore
            }
        default:
            return .ignore
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

#if os(tvOS)
/// The legibility contract for playback diagnostics viewed across a room.
/// These values stay separate from compact transport chrome because the info
/// surface is deliberately a dashboard, not another row of player controls.
enum TVPlaybackInfoPresentation {
    static let panelMaxWidth: CGFloat = 1_560
    static let titleFontSize: CGFloat = 42
    static let valueFontSize: CGFloat = 23
    static let cardMinimumHeight: CGFloat = 330

    static func healthLabel(stalls: Int?) -> String {
        guard let stalls else { return "Measuring" }
        if stalls == 0 { return "No stalls" }
        return stalls == 1 ? "1 stall" : "\(stalls) stalls"
    }
}
#endif

enum PlayerNaturalEndAction: Equatable {
    case dismiss
    case findNext

    @MainActor
    func perform(
        dismiss: () -> Void,
        findNext: () -> Void
    ) {
        switch self {
        case .dismiss: dismiss()
        case .findNext: findNext()
        }
    }
}

/// Makes every exit from the full-screen player follow the same order:
/// publish the restoring UI state, release playback resources exactly once,
/// then leave the cover on the next main-loop turn. That turn is important on
/// iPadOS — it gives the hosting controller a chance to restore status-bar,
/// Home-indicator, and persistent-overlay preferences before it disappears.
@MainActor
final class PlayerLifecycleCoordinator: ObservableObject {
    @Published private(set) var isTearingDown = false

    private var didTeardown = false
    private var didFinish = false
    private var pendingCompletions: [@MainActor () -> Void] = []
    private var completionDrainScheduled = false

    func teardown(_ cleanup: () -> Void) {
        guard !didTeardown else { return }
        didTeardown = true
        cleanup()
    }

    func finish(
        teardown cleanup: () -> Void,
        completion: @escaping @MainActor () -> Void
    ) {
        enqueueCompletion(completion)
        guard !didFinish else { return }
        didFinish = true
        isTearingDown = true
        teardown(cleanup)
    }

    /// Cleanup and the tearing-down state are write-once; completion ownership
    /// is not. Queue every caller, including one that arrives after `didFinish`,
    /// and deliver in order on the next main-loop turn.
    private func enqueueCompletion(_ completion: @escaping @MainActor () -> Void) {
        pendingCompletions.append(completion)
        guard !completionDrainScheduled else { return }
        completionDrainScheduled = true
        Task { @MainActor in
            await withCheckedContinuation {
                (continuation: CheckedContinuation<Void, Never>) in
                DispatchQueue.main.async { continuation.resume() }
            }
            let completions = pendingCompletions
            pendingCompletions.removeAll()
            completionDrainScheduled = false
            for completion in completions { completion() }
        }
    }
}

#if os(iOS)
struct PlayerSystemOverlayPreferences {
    let statusBarHidden: Bool
    let persistentOverlays: Visibility

    static let restoredAfterPlayback = Self(
        statusBarHidden: false,
        persistentOverlays: .automatic
    )

    static func resolve(
        controlsVisible: Bool,
        persistentContentVisible: Bool
    ) -> Self {
        let systemChromeVisible = controlsVisible || persistentContentVisible
        return Self(
            statusBarHidden: !systemChromeVisible,
            persistentOverlays: systemChromeVisible ? .automatic : .hidden
        )
    }
}

struct PlayerSystemOverlayModifier: ViewModifier {
    let preferences: PlayerSystemOverlayPreferences

    func body(content: Content) -> some View {
        content
            // The custom AVPlayerLayer surface has no AVPlayerViewController to
            // retire iPadOS chrome for it. Keep system chrome in the same state
            // as the controls the viewer actually asked to show.
            .statusBarHidden(preferences.statusBarHidden)
            .persistentSystemOverlays(preferences.persistentOverlays)
    }
}

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

/// The production marker content shared by the player and its layout tests.
struct PlayerMarkerButtonLabel: View {
    let title: String

    var body: some View {
        Label(title, systemImage: "forward.end.fill")
            .font(.system(.caption, design: .monospaced))
            .lineLimit(1)
    }
}

/// Keeps transient playback actions compact and pinned to the trailing edge of
/// the transport chrome at every viewport width.
struct PlayerTrailingControlRow<Control: View>: View {
    let control: Control

    init(@ViewBuilder control: () -> Control) {
        self.control = control()
    }

    var body: some View {
        HStack {
            Spacer(minLength: 0)
            control
                #if os(tvOS)
                .fixedSize(horizontal: true, vertical: false)
                #endif
        }
        .frame(maxWidth: .infinity)
    }
}

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
    var progressOffsetMs: Int = 0
    var itemDurationMs: Int? = nil
    var subtitle: String? = nil
    var year: Int? = nil
    var airDate: String? = nil
    var overview: String? = nil
    var offlineItem: OfflineItem? = nil
    /// The detail screen's pre-play track choice, spent on this playback only.
    var selection: PrePlaySelection = .none
    /// Debug acceptance can choose a deterministic first rung without racing a
    /// remote-control quality change against the initial session open.
    var initialHeight: Int? = nil
    var diagnosticProbesEnabled = false
    var onPlayNext: ((PlayContext) -> Void)?
    /// Hands the owning detail screen the last on-screen position immediately.
    /// The server progress write is intentionally best-effort and asynchronous;
    /// without this handoff the still-present detail view keeps rendering the
    /// resume point it loaded before playback began.
    var onPlaybackStopped: ((Int) -> Void)?

    @StateObject private var controller = PlayerController()
    @StateObject private var pictureInPicture = PictureInPictureController()
    @StateObject private var lifecycle = PlayerLifecycleCoordinator()
    @State private var showStats = false
    @State private var statsMode = PlaybackStatsMode.standard
    @State private var findingNext = false
    @State private var nextEpisodeTask: Task<Void, Never>?
    @State private var isScrubbing = false
    @State private var scrubMs = 0.0
    @State private var controlsVisible = true
    @State private var autoHideGeneration = 0
    #if os(iOS)
    @State private var activeOptionMenu: PlayerOptionMenu?
    #endif
    #if os(tvOS)
    @FocusState private var focusedControl: PlayerControl?
    @State private var tvProgressEngaged = false
    #endif

    var body: some View {
        ZStack(alignment: .topLeading) {
            Color.black.ignoresSafeArea()

            PlayerSurface(
                player: controller.player,
                pictureInPicture: pictureInPicture,
                pgsOverlay: controller.pgsOverlayWindow,
                allowsPictureInPicture: PlayerSurface.shouldAllowPictureInPicture(
                    isTearingDown: lifecycle.isTearingDown,
                    pgsOverlayIsActive: controller.pgsOverlayIsActive
                )
            )
                .ignoresSafeArea()

            #if os(tvOS)
            if lifecycle.isTearingDown {
                // Keep the presented cover in the focus system until the next
                // main-loop turn dismisses it. Removing every focusable child
                // first produces a transient "no focusable views" state.
                Color.clear
                    .contentShape(Rectangle())
                    .ignoresSafeArea()
                    .focusable()
                    .focusEffectDisabled()
                    .accessibilityHidden(true)
            }
            #endif

            if !lifecycle.isTearingDown {
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
                                #if os(tvOS)
                                // Playback info is a modal surface on the TV.
                                // Keep the remote inside its visible Done action
                                // instead of letting focus escape to dimmed
                                // transport controls behind the panel.
                                .disabled(showStats && statsMode != .mini)
                                #endif
                        }
                        .padding(20)
                        .transition(.opacity)
                    }

                    if overlayVisibility.playbackInfo {
                        #if os(tvOS)
                        PlaybackStatsView(
                            controller: controller,
                            mode: $statsMode,
                            onDismiss: dismissPlaybackInfo
                        )
                        .transition(.opacity.combined(with: .scale(scale: 0.98)))
                        #else
                        PlaybackStatsView(
                            controller: controller,
                            mode: $statsMode,
                            onDismiss: dismissPlaybackInfo
                        )
                        .frame(maxWidth: .infinity, alignment: .trailing)
                        .padding(20)
                        .transition(.opacity.combined(with: .move(edge: .trailing)))
                        #endif
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

                if let error = playbackBannerMessage,
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
                    progressOffsetMs: progressOffsetMs,
                    itemDurationMs: itemDurationMs,
                    title: title,
                    selection: selection,
                    initialHeight: initialHeight,
                    diagnosticProbesEnabled: diagnosticProbesEnabled
                )
            }
            #else
            controller.start(
                model: model,
                itemId: itemId,
                fileId: fileId,
                startMs: startMs,
                durationMs: durationMs,
                progressOffsetMs: progressOffsetMs,
                itemDurationMs: itemDurationMs,
                title: title,
                selection: selection,
                initialHeight: initialHeight,
                diagnosticProbesEnabled: diagnosticProbesEnabled
            )
            #endif
            #if os(tvOS)
            try? await Task.sleep(nanoseconds: 100_000_000)
            focusedControl = .playPause
            #endif
        }
        .onDisappear {
            lifecycle.teardown { teardownPlayback() }
        }
        .task(id: autoHideGeneration) {
            guard Self.shouldAutoHideControls(
                visible: controlsVisible,
                scrubbing: isScrubbing,
                changingStream: controller.isChangingStream,
                optionMenuOpen: optionMenuOpen,
                tearingDown: lifecycle.isTearingDown
            ) else { return }
            try? await Task.sleep(nanoseconds: Self.controlAutoHideDelayNanoseconds)
            guard !Task.isCancelled,
                  Self.shouldAutoHideControls(
                      visible: controlsVisible,
                      scrubbing: isScrubbing,
                      changingStream: controller.isChangingStream,
                      optionMenuOpen: optionMenuOpen,
                      tearingDown: lifecycle.isTearingDown
                  ) else { return }
            hideControls()
        }
        .onChange(of: controller.isPlaying) { _, _ in revealControls() }
        .onChange(of: controller.isChangingStream) { _, _ in revealControls() }
        .onChange(of: showStats) { _, _ in restartAutoHideTimer() }
        .onChange(of: isScrubbing) { _, _ in restartAutoHideTimer() }
        .onChange(of: optionMenuOpen) { _, _ in restartAutoHideTimer() }
        .onChange(of: controller.finished) { _, finished in
            let action = itemDurationMs != nil && offlineItem == nil
                ? Self.audiobookNaturalEndAction(
                    finished: finished,
                    alreadyFinding: findingNext
                )
                : Self.naturalEndAction(
                    finished: finished,
                    autoplay: model.autoplay,
                    offline: offlineItem != nil
                )
            guard let action else { return }
            // Leave the SwiftUI update transaction before publishing teardown
            // state and replacing the AVPlayer item.
            Task { @MainActor in
                await Task.yield()
                guard !lifecycle.isTearingDown else { return }
                handleNaturalEnd(action)
            }
        }
        #if os(tvOS)
        .onChange(of: controller.failed) { _, failed in
            guard failed else { return }
            focusedControl = controller.canRetryPlaybackFailure ? .retry : .close
        }
        .onChange(of: focusedControl) { _, newControl in
            if newControl != .progress { tvProgressEngaged = false }
            if controlsVisible { restartAutoHideTimer() }
        }
        .onExitCommand {
            if tvProgressEngaged {
                tvProgressEngaged = false
                revealControls()
            } else if showStats {
                dismissPlaybackInfo()
            } else if controlsVisible {
                hideControls()
            } else {
                finishPlayback()
            }
        }
        #endif
        #if os(iOS)
        .modifier(PlayerSystemOverlayModifier(
            preferences: playerSystemOverlayPreferences
        ))
        #endif
    }

    private func handleNaturalEnd(_ action: PlayerNaturalEndAction) {
        action.perform(
            dismiss: { finishPlayback() },
            findNext: {
                findingNext = true
                nextEpisodeTask?.cancel()
                nextEpisodeTask = Task {
                    let next: PlayContext?
                    if itemDurationMs != nil {
                        next = await model.nextAudiobookPart(itemId: itemId, after: fileId)
                    } else {
                        next = await model.nextEpisode(after: itemId)
                    }
                    guard !Task.isCancelled, !lifecycle.isTearingDown else { return }
                    findingNext = false
                    guard let next, let onPlayNext else {
                        finishPlayback()
                        return
                    }
                    finishPlayback(continuingPlayback: true) { onPlayNext(next) }
                }
            }
        )
    }

    private var playbackBannerMessage: String? {
        controller.playbackNotice
            ?? controller.playbackError
            ?? pictureInPicture.errorMessage
    }

    #if os(iOS)
    private var playerSystemOverlayPreferences: PlayerSystemOverlayPreferences {
        if lifecycle.isTearingDown {
            return .restoredAfterPlayback
        }
        return PlayerSystemOverlayPreferences.resolve(
            controlsVisible: controlsVisible,
            // These surfaces outlive the transport's four-second timeout. Keep
            // system chrome stable while the viewer reads them, then retire it
            // only after the last visible player surface has gone away.
            persistentContentVisible: showStats
                || controller.failed
                || controller.isChangingStream
                || findingNext
                || playbackBannerMessage != nil
        )
    }
    #endif

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
        optionMenuOpen: Bool,
        tearingDown: Bool = false
    ) -> Bool {
        visible && !scrubbing && !changingStream && !optionMenuOpen && !tearingDown
    }

    static func naturalEndAction(
        finished: Bool,
        autoplay: Bool,
        offline: Bool
    ) -> PlayerNaturalEndAction? {
        guard finished else { return nil }
        return autoplay && !offline ? .findNext : .dismiss
    }

    static func audiobookNaturalEndAction(
        finished: Bool,
        alreadyFinding: Bool
    ) -> PlayerNaturalEndAction? {
        guard finished, !alreadyFinding else { return nil }
        // Physical parts are one logical work, so crossing the seam does not
        // depend on the separate "autoplay next episode" preference.
        return .findNext
    }

    /// The one teardown path for natural completion, manual Close/Menu, and
    /// fatal-screen exits. UI state goes first so iPadOS restores system chrome
    /// while the cover still exists; AVKit and transport resources go next.
    private func finishPlayback(
        continuingPlayback: Bool = false,
        then completion: (@MainActor () -> Void)? = nil
    ) {
        lifecycle.finish(teardown: {
            teardownPlayback(deactivateAudioSession: !continuingPlayback)
        }) {
            if let completion {
                completion()
            } else {
                dismiss()
            }
        }
    }

    private func teardownPlayback(deactivateAudioSession: Bool = true) {
        let stoppedAt = controller.realPositionMs()
        nextEpisodeTask?.cancel()
        nextEpisodeTask = nil
        autoHideGeneration &+= 1
        showStats = false
        findingNext = false
        isScrubbing = false
        #if os(iOS)
        activeOptionMenu = nil
        #else
        tvProgressEngaged = false
        #endif
        pictureInPicture.detach()
        controller.stop(deactivateAudioSession: deactivateAudioSession)
        onPlaybackStopped?(stoppedAt)
    }

    /// A presented touch menu is hosted outside the control hierarchy. Removing
    /// the controls therefore removes its presentation anchor and dismisses the
    /// menu too. On tvOS, focus on a menu button is the equivalent interaction:
    /// keep the chrome up while the viewer is opening or navigating that menu.
    /// An engaged progress bar holds it open for the same reason.
    private var optionMenuOpen: Bool {
        #if os(iOS)
        activeOptionMenu != nil
        #else
        if tvProgressEngaged { return true }
        switch focusedControl {
        case .audio, .subtitles, .quality: return true
        default: return false
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
        guard !lifecycle.isTearingDown else { return }
        autoHideGeneration &+= 1
    }

    private func hideControls() {
        guard controlsVisible else { return }
        #if os(iOS)
        activeOptionMenu = nil
        #else
        tvProgressEngaged = false
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
        guard let seekDirection = Self.playerSeekDirection(direction) else { return }
        let outcome = TVPlayerRemoteRouting.moveOutcome(
            focusedControl: .reveal,
            progressEngaged: false,
            direction: seekDirection
        )
        applyRemoteMoveOutcome(outcome, revealingFromHiddenControls: true)
    }

    private static func playerSeekDirection(
        _ direction: MoveCommandDirection
    ) -> PlayerSeekDirection? {
        switch direction {
        case .left: .left
        case .right: .right
        case .up: .up
        case .down: .down
        @unknown default: nil
        }
    }

    private func applyRemoteMoveOutcome(
        _ outcome: PlayerRemoteMoveOutcome,
        revealingFromHiddenControls: Bool = false
    ) {
        switch outcome {
        case let .seek(seconds):
            controller.skip(seconds: seconds)
            if revealingFromHiddenControls {
                revealControlsFromRemote()
            } else {
                revealControls()
            }
        case let .focus(control):
            focusedControl = control
            revealControls()
        case .ignore:
            revealControls()
        }
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
            Text(controller.playbackFailureTitle)
                .font(.system(.body, design: .monospaced))
                .foregroundColor(.white)
            if let error = controller.playbackError {
                Text(error)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(Palette.muted)
                    .multilineTextAlignment(.center)
            }
            HStack(spacing: 12) {
                if controller.canRetryPlaybackFailure {
                    Button("Try Again") { controller.retryAfterPlaybackFailure() }
                        .buttonStyle(.borderedProminent)
                        .tint(Palette.accent)
                        #if os(tvOS)
                        .focused($focusedControl, equals: .retry)
                        #endif
                }
                Button("Close") { finishPlayback() }
                    .buttonStyle(.bordered)
                    #if os(tvOS)
                    .focused($focusedControl, equals: .close)
                    #endif
            }
        }
        .padding(30)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
    }

    #if os(iOS)
    private var closeButton: some View {
        Button { finishPlayback() } label: {
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
                PlayerTrailingControlRow {
                    markerButton(marker)
                }
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
            PlayerMarkerButtonLabel(title: marker.label)
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
                    // Seek first: it publishes the target into
                    // `controller.currentMs` synchronously, so the binding's
                    // fallback never flashes the pre-scrub position.
                    controller.seek(toMs: Int(scrubMs))
                    isScrubbing = false
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
        let controlState = pictureInPicture.controlState
        return Button {
            guard controller.allowsPictureInPictureCommand() else { return }
            pictureInPicture.toggle()
            revealControls()
        } label: {
            Image(systemName: pictureInPicture.isActive ? "pip.exit" : "pip.enter")
                .foregroundStyle(pictureInPicture.isActive ? Palette.accent : .white)
        }
        .disabled(!controlState.isButtonEnabled)
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
    /// Siri Remote's left/right commands as ordinary focus navigation until
    /// Select engages scrubbing; engaged presses move through the same
    /// absolute film timeline in 10-second steps. Up reaches a visible skip
    /// marker above the transport row and is inert when no marker is present;
    /// Down returns to play/pause. This is not a Button:
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
            .scaleEffect(y: tvProgressEngaged ? 1.5 : 1)
            .animation(.easeOut(duration: 0.12), value: tvProgressEngaged)
        }
        .frame(height: 8)
        .contentShape(Rectangle())
        .focusable()
        .focusEffectDisabled()
        .focused($focusedControl, equals: .progress)
        .onTapGesture {
            tvProgressEngaged.toggle()
            revealControls()
        }
        .onMoveCommand { direction in
            guard let seekDirection = Self.playerSeekDirection(direction) else { return }
            applyRemoteMoveOutcome(TVPlayerRemoteRouting.moveOutcome(
                focusedControl: .progress,
                progressEngaged: tvProgressEngaged,
                direction: seekDirection,
                progressRightNeighbor: progressRightControl,
                markerAvailable: controller.activeMarker != nil
            ))
        }
        .accessibilityLabel("Playback position")
        .accessibilityValue(tvProgressEngaged ? "Scrubbing" : "Not scrubbing")
        .accessibilityHint(tvProgressEngaged
            ? "Left or right seeks 10 seconds. Press Select or Menu to finish."
            : "Press Select to scrub. Left or right moves between controls.")
    }

    private var progressRightControl: PlayerControl {
        if pictureInPicture.isSupported,
           pictureInPicture.isActive || pictureInPicture.isPossible {
            return .pictureInPicture
        }
        if controller.audioTracks.count > 1 { return .audio }
        if !controller.subtitles.isEmpty { return .subtitles }
        if !controller.qualityRungs.isEmpty { return .quality }
        return .autoplay
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
        // Use an explicit color style so the popover cannot inherit that white.
        .foregroundStyle(Color(uiColor: PlayerOptionMenuPalette.foreground))
        .presentationCompactAdaptation(.popover)
    }

    private func optionMenuSection<Content: View>(
        _ title: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        Group {
            Text(title.uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color(uiColor: PlayerOptionMenuPalette.secondaryForeground))
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
                    Self.subtitleLabel(track),
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
        [track.title, Self.languageName(track.language), track.codec.uppercased(),
         track.channels.map { "\($0) ch" }]
            .compactMap { $0 }
            .joined(separator: "  ")
    }

    static func subtitleLabel(_ track: SubtitleTrack) -> String {
        var parts = [track.title, languageName(track.language), track.codec.uppercased()]
            .compactMap { $0 }
        if track.forced { parts.append("Forced") }
        if track.isPGSOverlay {
            parts.append("Overlay")
        } else if !track.isNativeHLS {
            parts.append("Burn-in")
        }
        return parts.joined(separator: "  ")
    }

    private static func languageName(_ code: String?) -> String? {
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

enum PlaybackStatsMode: String, CaseIterable, Identifiable {
    case mini
    case standard
    case debug

    var id: Self { self }

    var label: String {
        switch self {
        case .mini: return "Mini"
        case .standard: return "Standard"
        case .debug: return "Debug"
        }
    }
}

/// The same three playback-info levels used by the web and Android players.
/// Each client renders them natively, but Mini, Standard, and Debug keep the
/// same job and information hierarchy on every screen size.
private struct PlaybackStatsView: View {
    @ObservedObject var controller: PlayerController
    @Binding var mode: PlaybackStatsMode
    let onDismiss: () -> Void

    #if os(tvOS)
    @FocusState private var dismissFocused: Bool
    #endif

    /// This panel floats over the video, so its contrast must not follow the
    /// app's light/dark palette. A fixed dark surface keeps the white copy
    /// readable in every appearance and over both bright and dark frames.
    private let panelSurface = Palette.playerChrome.opacity(0.96)
    private let labelColor = Color.white.opacity(0.82)

    var body: some View {
        Group {
            if mode == .mini {
                miniBody
            } else if mode == .debug {
                debugBody
            } else {
                #if os(tvOS)
                televisionBody
                #else
                compactBody
                #endif
            }
        }
    }

    private var modeSelector: some View {
        HStack(spacing: 6) {
            ForEach(PlaybackStatsMode.allCases) { candidate in
                Button {
                    withAnimation(.easeInOut(duration: 0.16)) { mode = candidate }
                } label: {
                    Text(candidate.label)
                        .font(.system(size: modeFontSize, weight: .semibold, design: .rounded))
                        .padding(.horizontal, modeHorizontalPadding)
                        .padding(.vertical, modeVerticalPadding)
                        .foregroundStyle(mode == candidate ? .white : .white.opacity(0.62))
                        .background(
                            mode == candidate ? Palette.accent : Color.white.opacity(0.07),
                            in: Capsule()
                        )
                }
                .buttonStyle(.plain)
                .accessibilityLabel("\(candidate.label) playback info")
                .accessibilityAddTraits(mode == candidate ? .isSelected : [])
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Playback info size")
    }

    private var modeFontSize: CGFloat {
        #if os(tvOS)
        18
        #else
        12
        #endif
    }

    private var modeHorizontalPadding: CGFloat {
        #if os(tvOS)
        17
        #else
        11
        #endif
    }

    private var modeVerticalPadding: CGFloat {
        #if os(tvOS)
        10
        #else
        7
        #endif
    }

    private var miniBody: some View {
        VStack(alignment: .leading, spacing: miniSpacing) {
            HStack(spacing: miniSpacing) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(controller.methodLabel)
                        .font(.system(size: miniTitleSize, weight: .bold, design: .rounded))
                        .foregroundStyle(.white)
                        .lineLimit(1)
                    Text("\(formatTime(controller.currentMs)) / \(formatTime(controller.knownDurationMs))")
                        .font(.system(size: miniDetailSize, weight: .medium, design: .monospaced))
                        .foregroundStyle(.white.opacity(0.62))
                }

                Spacer(minLength: miniSpacing)
                miniHealth
                modeSelector
                closeButton
            }

            HStack(spacing: miniSpacing) {
                miniFact("Playing", miniPlayingSummary)
                miniFact("Buffer", miniBufferSummary)
                miniFact("Network", miniNetworkSummary)
            }
        }
        .padding(.horizontal, miniHorizontalPadding)
        .padding(.vertical, miniVerticalPadding)
        .frame(maxWidth: miniMaxWidth)
        .background(panelSurface, in: RoundedRectangle(cornerRadius: miniCornerRadius))
        .overlay {
            RoundedRectangle(cornerRadius: miniCornerRadius)
                .stroke(.white.opacity(0.12), lineWidth: 1)
        }
        .shadow(color: .black.opacity(0.45), radius: 24, y: 12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
        .padding(miniOuterPadding)
    }

    private var miniHealth: some View {
        let stalls = controller.stalls ?? 0
        return HStack(spacing: 7) {
            Circle()
                .fill(stalls > 0 ? Color.orange : Color.green)
                .frame(width: miniHealthDotSize, height: miniHealthDotSize)
            Text(stalls > 0 ? "\(stalls) stall\(stalls == 1 ? "" : "s")" : "Healthy")
                .font(.system(size: miniDetailSize, weight: .semibold, design: .rounded))
                .foregroundStyle(.white.opacity(0.82))
        }
        .accessibilityElement(children: .combine)
    }

    private func miniFact(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label.uppercased())
                .font(.system(size: miniLabelSize, weight: .bold, design: .rounded))
                .tracking(0.8)
                .foregroundStyle(.white.opacity(0.42))
            Text(value)
                .font(.system(size: miniDetailSize, weight: .semibold, design: .rounded))
                .foregroundStyle(.white)
                .lineLimit(1)
                .minimumScaleFactor(0.72)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var miniPlayingSummary: String {
        let size = controller.presentationSize
        let resolution = size.width > 0 && size.height > 0
            ? "\(Int(size.width))×\(Int(size.height))"
            : "Waiting"
        let range = PlayerView.dynamicRangeSummary(
            source: controller.decision?.source,
            delivered: controller.deliveredRange,
            displayHDR: Caps.displayIsHDR,
            reasons: []
        )
        return [resolution, range?.components(separatedBy: " — ").first]
            .compactMap { $0 }
            .joined(separator: " · ")
    }

    private var miniBufferSummary: String {
        if let runway = controller.bufferedRunwaySeconds() {
            return String(format: "%.1f s ahead", runway)
        }
        return "Measuring"
    }

    private var miniNetworkSummary: String {
        if let delivered = controller.sessionStatus?.deliveredBps, delivered > 0 {
            return bitRate(delivered)
        }
        if let observed = controller.observedBitrate, observed > 0 {
            return bitRate(Int(observed))
        }
        return "Measuring"
    }

    private var miniSpacing: CGFloat {
        #if os(tvOS)
        20
        #else
        10
        #endif
    }

    private var miniTitleSize: CGFloat {
        #if os(tvOS)
        25
        #else
        15
        #endif
    }

    private var miniDetailSize: CGFloat {
        #if os(tvOS)
        17
        #else
        11
        #endif
    }

    private var miniLabelSize: CGFloat {
        #if os(tvOS)
        13
        #else
        9
        #endif
    }

    private var miniHealthDotSize: CGFloat {
        #if os(tvOS)
        10
        #else
        7
        #endif
    }

    private var miniHorizontalPadding: CGFloat {
        #if os(tvOS)
        26
        #else
        14
        #endif
    }

    private var miniVerticalPadding: CGFloat {
        #if os(tvOS)
        20
        #else
        12
        #endif
    }

    private var miniMaxWidth: CGFloat {
        #if os(tvOS)
        1_380
        #else
        620
        #endif
    }

    private var miniCornerRadius: CGFloat {
        #if os(tvOS)
        22
        #else
        12
        #endif
    }

    private var miniOuterPadding: CGFloat {
        #if os(tvOS)
        54
        #else
        16
        #endif
    }

    private var closeButton: some View {
        Button(action: onDismiss) {
            Image(systemName: "xmark")
                .font(.system(size: modeFontSize, weight: .bold))
                .padding(modeVerticalPadding)
                .foregroundStyle(.white.opacity(0.72))
                .background(.white.opacity(0.07), in: Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Close playback info")
    }

    #if os(tvOS)
    private var televisionBody: some View {
        ZStack {
            Color.black.opacity(0.52)
                .ignoresSafeArea()

            VStack(alignment: .leading, spacing: 28) {
                televisionHeader

                HStack(alignment: .top, spacing: 22) {
                    sourceCard
                    outputCard
                    serverCard
                }

                if let reasons = controller.decision?.reasons, !reasons.isEmpty {
                    HStack(alignment: .firstTextBaseline, spacing: 14) {
                        Image(systemName: "info.circle.fill")
                            .foregroundStyle(Palette.accent)
                        Text(reasons.joined(separator: " · "))
                            .foregroundStyle(.white.opacity(0.78))
                            .lineLimit(2)
                    }
                    .font(.system(size: 20, weight: .medium, design: .rounded))
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel("Playback reason, \(reasons.joined(separator: "; "))")
                }
            }
            .padding(.horizontal, 44)
            .padding(.vertical, 38)
            .frame(maxWidth: TVPlaybackInfoPresentation.panelMaxWidth)
            .background(
                panelSurface,
                in: RoundedRectangle(cornerRadius: 28, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 28, style: .continuous)
                    .stroke(.white.opacity(0.12), lineWidth: 1)
            }
            .shadow(color: .black.opacity(0.55), radius: 38, y: 18)
            .padding(.horizontal, 90)
            .padding(.vertical, 70)
        }
        .onAppear {
            Task { @MainActor in
                await Task.yield()
                dismissFocused = true
            }
        }
    }

    private var televisionHeader: some View {
        HStack(alignment: .center, spacing: 28) {
            VStack(alignment: .leading, spacing: 8) {
                Text("Playback info")
                    .font(.system(
                        size: TVPlaybackInfoPresentation.titleFontSize,
                        weight: .bold,
                        design: .rounded
                    ))
                    .foregroundStyle(.white)

                HStack(spacing: 12) {
                    Text(controller.methodLabel)
                        .font(.system(size: 23, weight: .semibold, design: .rounded))
                        .foregroundStyle(Palette.accent)

                    Text("·")
                        .foregroundStyle(.white.opacity(0.35))

                    Text("\(formatTime(controller.currentMs)) of \(formatTime(controller.knownDurationMs))")
                        .font(.system(size: 22, weight: .medium, design: .rounded))
                        .foregroundStyle(.white.opacity(0.72))
                }
            }

            Spacer(minLength: 24)

            modeSelector

            playbackHealth

            Button(action: onDismiss) {
                Label("Done", systemImage: "xmark")
                    .font(.system(size: 21, weight: .semibold, design: .rounded))
            }
            .buttonStyle(TVReadableButtonStyle(prominent: false))
            .focused($dismissFocused)
            .accessibilityLabel("Close playback info")
        }
    }

    private var playbackHealth: some View {
        let stalls = controller.stalls
        let label = TVPlaybackInfoPresentation.healthLabel(stalls: stalls)
        let color: Color
        if let stalls, stalls > 0 {
            color = .orange
        } else if stalls == 0 {
            color = .green
        } else {
            color = .white.opacity(0.6)
        }
        return HStack(spacing: 9) {
            Circle()
                .fill(color)
                .frame(width: 11, height: 11)
            Text(label)
                .font(.system(size: 20, weight: .semibold, design: .rounded))
                .foregroundStyle(.white.opacity(0.86))
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.white.opacity(0.07), in: Capsule())
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Playback health, \(label)")
    }

    private var sourceCard: some View {
        televisionCard("Source", systemImage: "film.stack") {
            if let source = controller.decision?.source {
                let resolution = source.width.flatMap { width in
                    source.height.map { "\(width) × \($0)" }
                } ?? "Unknown"
                let video = [
                    source.videoCodec?.uppercased(),
                    source.bitDepth.map { "\($0)-bit" },
                    source.hdrFormat ?? source.hdr?.uppercased(),
                ]
                    .compactMap { $0 }
                    .joined(separator: " · ")
                televisionRow("Resolution", resolution)
                televisionRow("Video", video.isEmpty ? "Unknown" : video)
                televisionRow("Container", source.container?.uppercased() ?? "Unknown")
                if let bitrate = source.bitrate {
                    televisionRow("Bitrate", bitRate(bitrate))
                }
            } else {
                televisionEmpty("Waiting for source details")
            }
        }
    }

    private var outputCard: some View {
        televisionCard("Playing", systemImage: "play.rectangle.on.rectangle") {
            let size = controller.presentationSize
            if size.width > 0 && size.height > 0 {
                televisionRow("Output", "\(Int(size.width)) × \(Int(size.height))")
            }
            if let range = PlayerView.dynamicRangeSummary(
                source: controller.decision?.source,
                delivered: controller.deliveredRange,
                displayHDR: Caps.displayIsHDR,
                reasons: controller.decision?.reasons
            ) {
                televisionRow("Dynamic range", range)
            }
            if let bitrate = controller.observedBitrate, bitrate > 0 {
                televisionRow("Observed bitrate", bitRate(Int(bitrate)))
            } else if let bitrate = controller.indicatedBitrate, bitrate > 0 {
                televisionRow("Stream bitrate", bitRate(Int(bitrate)))
            }
            if let subtitle = controller.selectedSubtitle,
               let track = controller.subtitles.first(where: { $0.index == subtitle }) {
                televisionRow("Subtitles", subtitleDescription(track, index: subtitle))
            } else {
                televisionRow("Subtitles", "Off")
            }
        }
    }

    private var serverCard: some View {
        televisionCard("Server", systemImage: "server.rack") {
            if controller.isVOD && controller.methodLabel.contains("cached") {
                televisionRow("Status", "Served from cache")
            } else if let status = controller.sessionStatus {
                televisionRow(
                    "Status",
                    (status.suspended ?? false) ? "Holding buffer" : "Active"
                )
                if let encoder = status.encoder ?? controller.encoder {
                    televisionRow("Encoder", encoder)
                }
                if let speed = status.recentSpeed ?? status.speed {
                    televisionRow("Encode speed", String(format: "%.2f×", speed))
                }
                if let ahead = status.aheadSeconds {
                    let held = (status.suspended ?? false)
                        ? " · held\(holdReleaseDescription(status))"
                        : ""
                    televisionRow("Buffer ahead", "\(max(0, ahead)) s\(held)")
                }
                if let delivered = status.deliveredBps {
                    televisionRow("Delivery", bitRate(delivered))
                }
                if let bytes = status.deliveredBytes {
                    televisionRow("Transferred", byteCount(bytes))
                }
            } else {
                televisionEmpty("Waiting for server details")
            }
        }
    }

    private func televisionCard<Content: View>(
        _ title: String,
        systemImage: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            Label(title.uppercased(), systemImage: systemImage)
                .font(.system(size: 19, weight: .bold, design: .rounded))
                .tracking(1.15)
                .foregroundStyle(.white.opacity(0.58))

            content()
        }
        .padding(24)
        .frame(
            maxWidth: .infinity,
            minHeight: TVPlaybackInfoPresentation.cardMinimumHeight,
            alignment: .topLeading
        )
        .background(
            .white.opacity(0.055),
            in: RoundedRectangle(cornerRadius: 20, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 20, style: .continuous)
                .stroke(.white.opacity(0.08), lineWidth: 1)
        }
        .accessibilityElement(children: .contain)
    }

    private func televisionRow(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label)
                .font(.system(size: 17, weight: .medium, design: .rounded))
                .foregroundStyle(.white.opacity(0.48))
            Text(value)
                .font(.system(
                    size: TVPlaybackInfoPresentation.valueFontSize,
                    weight: .semibold,
                    design: .rounded
                ))
                .foregroundStyle(.white)
                .lineLimit(2)
                .minimumScaleFactor(0.82)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private func televisionEmpty(_ message: String) -> some View {
        Text(message)
            .font(.system(size: 22, weight: .medium, design: .rounded))
            .foregroundStyle(.white.opacity(0.48))
            .frame(maxWidth: .infinity, alignment: .leading)
    }
    #else
    private var compactBody: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 7) {
                HStack(spacing: 10) {
                    Text("Playback info")
                        .font(.system(.headline, design: .monospaced))
                        .foregroundColor(.white)
                    Spacer()
                    modeSelector
                    closeButton
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
    #endif

    private var debugBody: some View {
        ZStack {
            Color.black.opacity(0.52)
                .ignoresSafeArea()

            VStack(alignment: .leading, spacing: debugSpacing) {
                HStack(spacing: debugSpacing) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Playback debug")
                            .font(.system(size: debugTitleSize, weight: .bold, design: .rounded))
                            .foregroundStyle(.white)
                        Text("Live player, network, and server diagnostics")
                            .font(.system(size: debugDetailSize, weight: .medium, design: .rounded))
                            .foregroundStyle(.white.opacity(0.56))
                    }
                    Spacer(minLength: debugSpacing)
                    modeSelector
                    closeButton
                }

                Divider().overlay(.white.opacity(0.12))

                ScrollView {
                    LazyVGrid(
                        columns: [GridItem(.adaptive(minimum: debugColumnWidth), spacing: debugSpacing)],
                        alignment: .leading,
                        spacing: debugSpacing
                    ) {
                        debugPlaybackSection
                        debugSourceSection
                        debugDecodingSection
                        debugNetworkSection
                        debugServerSection
                    }
                    .padding(.bottom, 2)
                }
            }
            .padding(debugPanelPadding)
            .frame(maxWidth: debugMaxWidth, maxHeight: debugMaxHeight)
            .background(
                panelSurface,
                in: RoundedRectangle(cornerRadius: debugCornerRadius, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: debugCornerRadius, style: .continuous)
                    .stroke(.white.opacity(0.13), lineWidth: 1)
            }
            .shadow(color: .black.opacity(0.58), radius: 36, y: 18)
            .padding(debugOuterPadding)
        }
    }

    private var debugPlaybackSection: some View {
        debugSection("Playback") {
            debugRow("Build", buildLabel)
            debugRow("Method", controller.methodLabel)
            debugRow(
                "Transport",
                controller.currentSessionId == nil
                    ? "Continuous file · range requests · Apple AVPlayer"
                    : "Segmented HLS · Apple AVPlayer"
            )
            debugRow(
                "Position",
                "\(formatTime(controller.currentMs)) / \(formatTime(controller.knownDurationMs))"
            )
            debugRow("File ID", controller.decision.map { "#\($0.fileId)" } ?? "—")
            if let session = controller.currentSessionId {
                debugRow("Session", session)
            }
            if let reasons = controller.decision?.reasons, !reasons.isEmpty {
                debugRow("Reason", reasons.joined(separator: "; "))
            }
        }
    }

    private var debugSourceSection: some View {
        debugSection("Source") {
            if let source = controller.decision?.source {
                let video = [
                    source.videoCodec?.uppercased(),
                    source.videoProfile,
                    source.bitDepth.map { "\($0)-bit" },
                    source.hdrFormat ?? source.hdr?.uppercased(),
                ].compactMap { $0 }.joined(separator: " · ")
                debugRow("Video", video.isEmpty ? "—" : video)
                debugRow(
                    "Resolution",
                    source.width.flatMap { width in source.height.map { "\(width)×\($0)" } } ?? "—"
                )
                debugRow("Bitrate", source.bitrate.map(bitRate) ?? "—")
                debugRow("Container", source.container?.uppercased() ?? "—")
            }
            if let audio = selectedAudioDescription {
                debugRow("Audio", audio)
            }
            debugRow("AV offset", "\(controller.decision?.audioOffsetMs ?? 0) ms")
        }
    }

    private var debugDecodingSection: some View {
        let snapshot = controller.currentDiagnosticSnapshot
        return debugSection("Now decoding") {
            let size = controller.presentationSize
            debugRow(
                "Resolution",
                size.width > 0 && size.height > 0
                    ? "\(Int(size.width))×\(Int(size.height))"
                    : "—"
            )
            if let range = PlayerView.dynamicRangeSummary(
                source: controller.decision?.source,
                delivered: controller.deliveredRange,
                displayHDR: Caps.displayIsHDR,
                reasons: controller.decision?.reasons
            ) {
                debugRow("Dynamic range", range)
            }
            debugRow("Buffer", snapshot.runway.map { String(format: "%.1f s", $0) } ?? "—")
            debugRow("Player state", snapshot.timeControlStatus ?? "unknown")
            if let waiting = snapshot.waitingReason { debugRow("Waiting reason", waiting) }
            debugRow("Buffer empty", yesNo(snapshot.playbackBufferEmpty))
            debugRow("Likely to keep up", yesNo(snapshot.playbackLikelyToKeepUp))
            debugRow("Buffer full", yesNo(snapshot.playbackBufferFull))
            debugRow("Stalls", snapshot.accessStalls.map(String.init) ?? "—")
            debugRow("Subtitles", selectedSubtitleDescription)
        }
    }

    private var debugNetworkSection: some View {
        let snapshot = controller.currentDiagnosticSnapshot
        return debugSection("Network") {
            debugRow("Delivery rate", controller.sessionStatus?.deliveredBps.map(bitRate) ?? "—")
            debugRow("Observed rate", snapshot.observedBitrateBps.map { bitRate(Int($0)) } ?? "—")
            debugRow("Stream rate", snapshot.indicatedBitrateBps.map { bitRate(Int($0)) } ?? "—")
            debugRow("Requests", snapshot.mediaRequests.map(String.init) ?? "—")
            debugRow(
                "Downloaded media",
                snapshot.downloadedDuration.map { String(format: "%.1f s", $0) } ?? "—"
            )
            debugRow("Transferred", snapshot.bytesTransferred.map(byteCount) ?? "—")
            debugRow(
                "Transfer time",
                snapshot.transferDuration.map { String(format: "%.2f s", $0) } ?? "—"
            )
        }
    }

    private var debugServerSection: some View {
        debugSection("Server") {
            if controller.isVOD {
                debugRow("Stream", "Already transcoded · served from cache")
            } else if let status = controller.sessionStatus {
                debugRow("Encoder", status.encoder ?? controller.encoder ?? "—")
                debugRow(
                    "Encode speed",
                    (status.recentSpeed ?? status.speed).map { String(format: "%.2f×", $0) } ?? "—"
                )
                debugRow("Server ahead", status.aheadSeconds.map { "\(max(0, $0)) s" } ?? "—")
                debugRow("Ahead bytes", status.aheadBytes.map(byteCount) ?? "—")
                debugRow("Produced", status.outTimeMs.map { formatTime($0) } ?? "—")
                debugRow("Pacing", status.readrate.map { String(format: "%.2f×", $0) } ?? "—")
                debugRow("Held", yesNo(status.suspended))
                if let reason = status.holdReason { debugRow("Hold reason", reason) }
                debugRow("Suspend count", status.suspendCount.map(String.init) ?? "0")
                debugRow("Delivered", status.deliveredBytes.map(byteCount) ?? "—")
                debugRow("Delivery idle", status.deliveredIdleMs.map { "\($0) ms" } ?? "—")
                if let request = status.lastRequest { debugRow("Last request", request) }
                debugRow("Request idle", status.idleSeconds.map { "\($0) s" } ?? "—")
                if let shape = status.playlistShape { debugRow("Playlist", shape) }
                debugRow("Published end", status.publishedEndMs.map { "\($0) ms" } ?? "—")
                debugRow("Fetched end", status.fetchedEndMs.map { "\($0) ms" } ?? "—")
            } else {
                debugRow("Status", "No server-side session")
            }
        }
    }

    private func debugSection<Content: View>(
        _ title: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: debugRowSpacing) {
            Text(title.uppercased())
                .font(.system(size: debugSectionSize, weight: .bold, design: .rounded))
                .tracking(1)
                .foregroundStyle(Palette.accent)
            content()
        }
        .padding(debugSectionPadding)
        .frame(maxWidth: .infinity, alignment: .topLeading)
        .background(
            .white.opacity(0.045),
            in: RoundedRectangle(cornerRadius: debugSectionCornerRadius, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: debugSectionCornerRadius, style: .continuous)
                .stroke(.white.opacity(0.07), lineWidth: 1)
        }
    }

    private func debugRow(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label)
                .font(.system(size: debugLabelSize, weight: .medium, design: .monospaced))
                .foregroundStyle(.white.opacity(0.45))
            #if os(iOS)
            Text(value)
                .font(.system(size: debugValueSize, weight: .medium, design: .monospaced))
                .foregroundStyle(.white.opacity(0.92))
                .textSelection(.enabled)
            #else
            Text(value)
                .font(.system(size: debugValueSize, weight: .medium, design: .monospaced))
                .foregroundStyle(.white.opacity(0.92))
            #endif
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var selectedAudioDescription: String? {
        let track = controller.audioTracks.first(where: { $0.index == controller.selectedAudio })
            ?? controller.audioTracks.first(where: { $0.default })
        guard let track else { return nil }
        return [
            track.codec.uppercased(),
            track.channels.map(channelDescription),
            track.language?.uppercased(),
            track.title,
        ].compactMap { $0 }.joined(separator: " · ")
    }

    private var selectedSubtitleDescription: String {
        guard let index = controller.selectedSubtitle,
              let track = controller.subtitles.first(where: { $0.index == index })
        else { return "Off" }
        return subtitleDescription(track, index: index)
    }

    private func channelDescription(_ channels: Int) -> String {
        switch channels {
        case 1: return "Mono"
        case 2: return "Stereo"
        case 6: return "5.1"
        case 8: return "7.1"
        default: return "\(channels)ch"
        }
    }

    private func yesNo(_ value: Bool?) -> String {
        value.map { $0 ? "Yes" : "No" } ?? "—"
    }

    private var buildLabel: String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String
        switch (version, build) {
        case let (version?, build?) where version != build: return "\(version) (\(build))"
        case let (version?, _): return version
        case let (_, build?): return build
        default: return "development"
        }
    }

    private var debugSpacing: CGFloat {
        #if os(tvOS)
        22
        #else
        12
        #endif
    }

    private var debugRowSpacing: CGFloat {
        #if os(tvOS)
        13
        #else
        8
        #endif
    }

    private var debugTitleSize: CGFloat {
        #if os(tvOS)
        34
        #else
        20
        #endif
    }

    private var debugDetailSize: CGFloat {
        #if os(tvOS)
        18
        #else
        12
        #endif
    }

    private var debugSectionSize: CGFloat {
        #if os(tvOS)
        17
        #else
        11
        #endif
    }

    private var debugLabelSize: CGFloat {
        #if os(tvOS)
        15
        #else
        10
        #endif
    }

    private var debugValueSize: CGFloat {
        #if os(tvOS)
        17
        #else
        11
        #endif
    }

    private var debugColumnWidth: CGFloat {
        #if os(tvOS)
        360
        #else
        260
        #endif
    }

    private var debugPanelPadding: CGFloat {
        #if os(tvOS)
        32
        #else
        16
        #endif
    }

    private var debugSectionPadding: CGFloat {
        #if os(tvOS)
        20
        #else
        12
        #endif
    }

    private var debugMaxWidth: CGFloat {
        #if os(tvOS)
        1_700
        #else
        820
        #endif
    }

    private var debugMaxHeight: CGFloat {
        #if os(tvOS)
        900
        #else
        720
        #endif
    }

    private var debugCornerRadius: CGFloat {
        #if os(tvOS)
        28
        #else
        14
        #endif
    }

    private var debugSectionCornerRadius: CGFloat {
        #if os(tvOS)
        18
        #else
        10
        #endif
    }

    private var debugOuterPadding: CGFloat {
        #if os(tvOS)
        54
        #else
        12
        #endif
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
                let release = holdReleaseDescription(status)
                row("Server ahead", "\(max(0, ahead)) s\((status.suspended ?? false) ? " · held\(release)" : "")")
            }
            if let delivered = status.deliveredBps { row("Delivery rate", bitRate(delivered)) }
            if let bytes = status.deliveredBytes { row("Delivered", byteCount(bytes)) }
        }
        if let subtitle = controller.selectedSubtitle,
           let track = controller.subtitles.first(where: { $0.index == subtitle }) {
            row("Subtitles", subtitleDescription(track, index: subtitle))
        }
    }

    private func subtitleDescription(_ track: SubtitleTrack, index: Int) -> String {
        let delivery = track.isPGSOverlay
            ? (controller.pgsOverlayStatus.label ?? "PGS overlay")
            : (track.isNativeHLS ? "native WebVTT" : "burned in")
        return (track.title ?? track.language?.uppercased() ?? "Track \(index + 1)")
            + " · \(delivery)"
    }

    private func holdReleaseDescription(_ status: PlaybackSessionStatus) -> String {
        guard status.suspended ?? false else { return "" }
        switch status.holdReason {
        case "time":
            return status.resumeBelowSeconds.map { " · time release ≤\($0) s" } ?? ""
        case "bytes":
            return status.resumeBelowBytes.map { " · bytes release ≤\(byteCount($0))" } ?? ""
        case "global":
            return status.resumeBelowBytes.map { " · global release ≤\(byteCount($0))" } ?? ""
        default:
            return ""
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
