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

struct PlayerMetadataBadge: Equatable, Identifiable {
    enum Kind: String, Equatable {
        case resolution
        case dynamicRange
        case audio
    }

    let kind: Kind
    let symbol: String
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

    var id: String { kind.rawValue }
}

enum PlayerMetadataBadgeMetrics {
    static let rowSpacing: CGFloat = 6
    static let contentSpacing: CGFloat = 4
    static let horizontalPadding: CGFloat = 6
    static let verticalPadding: CGFloat = 2
    static let strokeWidth: CGFloat = 0.5
    /// Readable, but plainly the "off" treatment next to a lit chip.
    static let dimmedOpacity: Double = 0.45

    #if os(tvOS)
    static let fontSize: CGFloat = 16
    #endif
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

    /// The base chip text and its spoken form, unchanged from what the overlay
    /// has always shown for a source: the arrow suffix carries the precision.
    static func sourceMark(_ grade: String) -> String {
        grade == dolbyVision ? "DV" : "HDR"
    }

    static func sourceLabel(_ grade: String) -> String {
        grade == dolbyVision ? "Dolby Vision" : "HDR"
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
    var overview: String? = nil
    var onPlayNext: ((PlayContext) -> Void)?
    /// Hands the owning detail screen the last on-screen position immediately.
    /// The server progress write is intentionally best-effort and asynchronous;
    /// without this handoff the still-present detail view keeps rendering the
    /// resume point it loaded before playback began.
    var onPlaybackStopped: ((Int) -> Void)?

    @StateObject private var controller = PlayerController()
    @StateObject private var pictureInPicture = PictureInPictureController()
    @State private var showStats = false
    @State private var showNowPlayingInfo = false
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

            PlayerSurface(player: controller.player, pictureInPicture: pictureInPicture)
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
                    .onMoveCommand { _ in revealControlsFromRemote() }
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

            if let error = controller.playbackError ?? pictureInPicture.errorMessage,
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
                changingStream: controller.isChangingStream
            ) else { return }
            try? await Task.sleep(nanoseconds: Self.controlAutoHideDelayNanoseconds)
            guard !Task.isCancelled,
                  Self.shouldAutoHideControls(
                      visible: controlsVisible,
                      scrubbing: isScrubbing,
                      changingStream: controller.isChangingStream
                  ) else { return }
            hideControls()
        }
        .onChange(of: controller.isPlaying) { _, _ in revealControls() }
        .onChange(of: controller.isChangingStream) { _, _ in revealControls() }
        .onChange(of: showStats) { _, _ in restartAutoHideTimer() }
        .onChange(of: showNowPlayingInfo) { _, _ in restartAutoHideTimer() }
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
        .onChange(of: focusedControl) { _, _ in
            if controlsVisible { restartAutoHideTimer() }
        }
        .onExitCommand {
            if controlsVisible {
                hideControls()
            } else {
                dismiss()
            }
        }
        #endif
    }

    private var shouldShowControls: Bool {
        controlsVisible
    }

    static func shouldAutoHideControls(
        visible: Bool,
        scrubbing: Bool,
        changingStream: Bool
    ) -> Bool {
        visible && !scrubbing && !changingStream
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
        withAnimation(.easeInOut(duration: 0.2)) { controlsVisible = true }
        restartAutoHideTimer()
    }

    private func restartAutoHideTimer() {
        autoHideGeneration &+= 1
    }

    private func hideControls() {
        guard controlsVisible else { return }
        withAnimation(.easeOut(duration: 0.2)) {
            controlsVisible = false
            showStats = false
            showNowPlayingInfo = false
        }
        #if os(tvOS)
        focusedControl = nil
        Task { @MainActor in
            await Task.yield()
            if !controlsVisible { focusedControl = .reveal }
        }
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
        VStack(alignment: .leading, spacing: 6) {
            #if os(tvOS)
            playbackInfoHeader
            #else
            playbackInfoHeader
            #endif

            if controller.knownDurationMs > 0 {
                HStack(spacing: 10) {
                    playbackTimeLabel(Int(isScrubbing ? scrubMs : Double(controller.currentMs)))
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
                    playbackTimeLabel(controller.knownDurationMs)
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
            if showNowPlayingInfo {
                nowPlayingInfoSection
                    .transition(.opacity.combined(with: .move(edge: .bottom)))
            } else if let cue = Self.nowPlayingInfoCueLabel(showingInfo: showNowPlayingInfo) {
                nowPlayingInfoCue(cue)
                    .transition(.opacity)
            }
            #else
            ViewThatFits(in: .horizontal) {
                expandedControlRow
                    .fixedSize(horizontal: true, vertical: false)
                compactControlRow
            }
            .frame(maxWidth: .infinity)
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
        .buttonStyle(.bordered)
        .tint(.white)
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
        #endif
    }

    #if os(tvOS)
    private func nowPlayingInfoCue(_ label: String) -> some View {
        Label(label, systemImage: "chevron.down")
            .font(.system(size: 13, weight: .bold, design: .monospaced))
            .foregroundStyle(.white.opacity(0.72))
            .padding(.horizontal, 9)
            .padding(.vertical, 3)
            .background(.black.opacity(0.48), in: Capsule())
            .overlay {
                Capsule().stroke(.white.opacity(0.16), lineWidth: 0.5)
            }
            .frame(maxWidth: .infinity)
            .accessibilityLabel("Press Down for information about what is playing")
    }

    private var nowPlayingInfoSection: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text("NOW PLAYING")
                .font(.system(
                    size: TVPlayerChromeMetrics.infoHeadingFontSize,
                    weight: .bold,
                    design: .monospaced
                ))
                .foregroundStyle(Palette.accent)
            Text(Self.nowPlayingSummary(overview))
                .font(.system(size: TVPlayerChromeMetrics.infoBodyFontSize, weight: .regular))
                .foregroundStyle(.white.opacity(0.88))
                .lineLimit(TVPlayerChromeMetrics.infoLineLimit)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .frame(maxWidth: 980, alignment: .leading)
        .background(
            Palette.playerChrome.opacity(0.68),
            in: RoundedRectangle(cornerRadius: 8, style: .continuous)
        )
        .accessibilityElement(children: .combine)
    }

    private func revealNowPlayingInfo() {
        guard controlsVisible else { return }
        withAnimation(.easeOut(duration: 0.18)) { showNowPlayingInfo = true }
        restartAutoHideTimer()
    }
    #endif

    static func nowPlayingSummary(_ overview: String?) -> String {
        let summary = overview?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return summary.isEmpty ? "No description available." : summary
    }

    static func nowPlayingInfoCueLabel(showingInfo: Bool) -> String? {
        showingInfo ? nil : "INFO"
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
        #if os(tvOS)
        HStack(alignment: .firstTextBaseline, spacing: 18) {
            playbackIdentity
                .padding(.horizontal, TVPlayerChromeMetrics.headerHorizontalInset)
                .padding(.vertical, TVPlayerChromeMetrics.headerVerticalInset)
                .background(
                    Palette.playerChrome.opacity(0.68),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                )
                .layoutPriority(1)
            Spacer(minLength: 18)
            playbackFacts
                .fixedSize(horizontal: true, vertical: false)
        }
        .frame(maxWidth: .infinity)
        #else
        VStack(alignment: .leading, spacing: 7) {
            playbackIdentity
            playbackFacts
        }
        #endif
    }

    private var playbackIdentity: some View {
        let contextParts = [subtitle, year.map(String.init), runtimeLabel]
            .compactMap { $0 }
            .filter { !$0.isEmpty }

        #if os(tvOS)
        let context = contextParts.joined(separator: "   ·   ")
        return (
            Text(title)
                .font(.system(size: 26, weight: .semibold))
                .foregroundColor(.white)
            + Text(context.isEmpty ? "" : "   ·   \(context)")
                .font(.system(size: 22, design: .monospaced))
                .foregroundColor(.white.opacity(0.76))
        )
        .lineLimit(1)
        #else
        return VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.headline.bold())
                .foregroundColor(.white)
                .lineLimit(1)

            if !contextParts.isEmpty {
                HStack(spacing: 12) {
                    ForEach(contextParts, id: \.self) { fact in
                        Text(fact)
                    }
                }
                .font(.system(.caption, design: .rounded).weight(.medium))
                .foregroundColor(.white.opacity(0.7))
                .lineLimit(1)
            }
        }
        #endif
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
                HStack(spacing: PlayerMetadataBadgeMetrics.contentSpacing) {
                    HStack(spacing: PlayerMetadataBadgeMetrics.contentSpacing) {
                        Image(systemName: badge.symbol)
                            .imageScale(.small)
                        if let mark = badge.mark {
                            Text(mark)
                                .fontWeight(.semibold)
                        }
                    }
                    .opacity(badge.dimmed ? PlayerMetadataBadgeMetrics.dimmedOpacity : 1)
                    if let renderedMark = badge.renderedMark {
                        Text("→ \(renderedMark)")
                            .fontWeight(.semibold)
                    }
                }
                .padding(.horizontal, PlayerMetadataBadgeMetrics.horizontalPadding)
                .padding(.vertical, PlayerMetadataBadgeMetrics.verticalPadding)
                .background(.black.opacity(0.32), in: Capsule())
                .overlay {
                    Capsule().stroke(
                        .white.opacity(0.18),
                        lineWidth: PlayerMetadataBadgeMetrics.strokeWidth
                    )
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(badge.accessibilityLabel)
            }
        }
        #if os(tvOS)
        .font(.system(
            size: PlayerMetadataBadgeMetrics.fontSize,
            weight: .medium,
            design: .rounded
        ))
        #else
        .font(.system(.caption2, design: .rounded).weight(.medium))
        #endif
        .foregroundColor(.white.opacity(0.78))
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
        if let label = resolutionLabel(width: source?.width, height: source?.height) {
            let is4K = label == "4K"
            badges.append(PlayerMetadataBadge(
                kind: .resolution,
                symbol: is4K ? "4k.tv.fill" : "tv.fill",
                mark: is4K ? nil : label.uppercased(),
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
                symbol: "waveform",
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
        let lit = PlayerMetadataBadge(
            kind: .dynamicRange,
            symbol: "sparkles",
            mark: DynamicRange.sourceMark(source),
            accessibilityLabel: DynamicRange.sourceLabel(source)
        )
        guard let delivered, !delivered.isEmpty else { return lit }
        let rendered = DynamicRange.rendered(delivered: delivered, displayHDR: displayHDR)
        guard rendered != source else { return lit }
        return PlayerMetadataBadge(
            kind: .dynamicRange,
            symbol: "sparkles",
            mark: DynamicRange.sourceMark(source),
            accessibilityLabel:
                "\(DynamicRange.sourceLabel(source)), playing as \(DynamicRange.longLabel(rendered))",
            renderedMark: DynamicRange.shortLabel(rendered),
            dimmed: true
        )
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
        case 1: channels = ("1.0", "mono")
        case 2: channels = ("2.0", "stereo")
        case 6: channels = ("5.1", "5.1 channels")
        case 8: channels = ("7.1", "7.1 channels")
        case let count?: channels = ("\(count)CH", "\(count) channels")
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
        #if os(tvOS)
        .onMoveCommand { direction in
            if direction == .down { revealNowPlayingInfo() }
        }
        #endif
    }

    private var transportControlGroup: some View {
        HStack(spacing: 8) {
            skipBackButton
            playPauseButton
            skipForwardButton
        }
        #if os(iOS)
        .padding(.horizontal, 5)
        .padding(.vertical, 4)
        .background(.black.opacity(0.14), in: RoundedRectangle(cornerRadius: 12))
        #endif
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
        #if os(iOS)
        .padding(.horizontal, 5)
        .padding(.vertical, 4)
        .background(.black.opacity(0.14), in: RoundedRectangle(cornerRadius: 12))
        #endif
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
    /// film timeline in 30-second steps. This is deliberately not a Button:
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
            case .left: controller.skip(seconds: -30)
            case .right: controller.skip(seconds: 30)
            case .down: focusedControl = .playPause
            default: break
            }
            revealControls()
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
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
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
        .focused($focusedControl, equals: .subtitles)
        #endif
    }

    private var qualityMenu: some View {
        Menu { qualityChoices } label: {
            Image(systemName: "slider.horizontal.3")
        }
        .accessibilityLabel("Playback quality")
        #if os(tvOS)
        .buttonStyle(TVPlayerControlButtonStyle())
        .focusEffectDisabled()
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
            let delivery = track.isNativeHLS ? "native WebVTT" : "burned in"
            row("Subtitles", (track.title ?? track.language?.uppercased() ?? "Track \(subtitle + 1)") + " · \(delivery)")
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
