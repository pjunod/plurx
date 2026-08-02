import SwiftUI

struct ItemMetadataBadge: Equatable, Identifiable {
    enum Kind: String, Equatable {
        case series
        case episode
        case year
        case runtime
        case resolution
        case video
    }

    let kind: Kind
    let symbol: String
    let mark: String?
    let accessibilityLabel: String

    var id: String { kind.rawValue }
}

struct ItemMetadataBadgeRow: View {
    let badges: [ItemMetadataBadge]

    var body: some View {
        #if os(tvOS)
        badgeContent
        #else
        ScrollView(.horizontal, showsIndicators: false) {
            badgeContent
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .clipped()
        #endif
    }

    private var badgeContent: some View {
        HStack(spacing: 9) {
            ForEach(badges) { badge in
                HStack(spacing: 6) {
                    Image(systemName: badge.symbol)
                    if let mark = badge.mark {
                        Text(mark)
                            .fontWeight(.semibold)
                    }
                }
                .padding(.horizontal, 9)
                .padding(.vertical, 5)
                .background(Palette.surfaceHi.opacity(0.84), in: Capsule())
                .overlay {
                    Capsule().stroke(Palette.outline.opacity(0.72), lineWidth: 0.5)
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(badge.accessibilityLabel)
            }
        }
        #if os(tvOS)
        .font(.system(size: 19, weight: .medium, design: .rounded))
        #else
        .font(.system(.callout, design: .rounded))
        #endif
        .foregroundColor(Palette.onBg.opacity(0.86))
    }
}

/// Clickable ancestors for a detail page. The server returns these outermost
/// first, so an episode naturally reads "Show / Season" and a season reads
/// "Show". The current title sits immediately below the trail.
struct DetailBreadcrumb: View {
    let ancestors: [Item]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: DetailBreadcrumbMetrics.itemSpacing) {
                ForEach(ancestors.indices, id: \.self) { index in
                    if index > ancestors.startIndex {
                        Text("/")
                            .foregroundColor(Palette.muted)
                            .accessibilityHidden(true)
                    }

                    let ancestor = ancestors[index]
                    NavigationLink(value: Self.destination(for: ancestor)) {
                        Text(ancestor.title)
                            .lineLimit(1)
                    }
                    .breadcrumbButtonStyle()
                    .accessibilityHint("Open \(ancestor.kind)")
                }
            }
            .breadcrumbFont()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .clipped()
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Show path")
    }

    static func destination(for ancestor: Item) -> Route {
        .item(ancestor.id)
    }
}

enum DetailBreadcrumbMetrics {
    static let itemSpacing: CGFloat = 6
    static let horizontalPadding: CGFloat = 8
    static let verticalPadding: CGFloat = 4
    static let focusStrokeWidth: CGFloat = 1
}

private struct DetailBreadcrumbLinkStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        #if os(tvOS)
        TVBody(configuration: configuration)
        #else
        configuration.label
            .foregroundStyle(Palette.accent)
            .padding(.horizontal, 6)
            .padding(.vertical, 4)
            .opacity(configuration.isPressed ? 0.66 : 1)
        #endif
    }

    #if os(tvOS)
    private struct TVBody: View {
        let configuration: ButtonStyle.Configuration
        @Environment(\.isFocused) private var isFocused

        var body: some View {
            configuration.label
                .foregroundStyle(isFocused ? Palette.onBg : Palette.accent)
                .padding(.horizontal, DetailBreadcrumbMetrics.horizontalPadding)
                .padding(.vertical, DetailBreadcrumbMetrics.verticalPadding)
                .background(
                    Palette.surfaceHi.opacity(isFocused ? 0.9 : 0),
                    in: Capsule()
                )
                .overlay {
                    Capsule()
                        .stroke(
                            Palette.accent.opacity(isFocused ? 0.9 : 0),
                            lineWidth: DetailBreadcrumbMetrics.focusStrokeWidth
                        )
                }
                .contentShape(Capsule())
                .scaleEffect(configuration.isPressed ? 0.98 : 1)
                .animation(.easeOut(duration: 0.12), value: isFocused)
        }
    }
    #endif
}

private extension View {
    @ViewBuilder
    func breadcrumbButtonStyle() -> some View {
        #if os(tvOS)
        self
            .buttonStyle(DetailBreadcrumbLinkStyle())
            .focusEffectDisabled()
        #else
        self.buttonStyle(DetailBreadcrumbLinkStyle())
        #endif
    }

    @ViewBuilder
    func breadcrumbFont() -> some View {
        #if os(tvOS)
        self.font(.system(size: 20, weight: .semibold, design: .rounded))
        #else
        self.font(.system(.subheadline, design: .rounded).weight(.semibold))
        #endif
    }
}

/// Identifies a play request for the full-screen player cover.
struct PlayContext: Identifiable {
    let id = UUID()
    let itemId: Int
    let fileId: Int
    let startMs: Int
    let durationMs: Int
    let title: String
    var subtitle: String? = nil
    var year: Int? = nil
    var overview: String? = nil
}

enum DetailLayoutMetrics {
    static let maximumBodyWidth: CGFloat = 980 - (2 * screenHPad)

    static func bodyWidth(in viewportWidth: CGFloat) -> CGFloat {
        min(maximumBodyWidth, max(0, viewportWidth - (2 * screenHPad)))
    }
}

private struct DetailViewportWidthKey: EnvironmentKey {
    static let defaultValue: CGFloat? = nil
}

private extension EnvironmentValues {
    var detailViewportWidth: CGFloat? {
        get { self[DetailViewportWidthKey.self] }
        set { self[DetailViewportWidthKey.self] = newValue }
    }
}

/// Keeps the readable detail column centered on large screens without ever
/// growing wider than the compact device that contains it.
struct DetailBodyFrame<Content: View>: View {
    @Environment(\.detailViewportWidth) private var viewportWidth
    @ViewBuilder let content: Content

    @ViewBuilder
    var body: some View {
        if let viewportWidth {
            content
                .frame(
                    width: DetailLayoutMetrics.bodyWidth(in: viewportWidth),
                    alignment: .leading
                )
                .frame(width: viewportWidth, alignment: .center)
        } else {
            content
                .frame(maxWidth: DetailLayoutMetrics.maximumBodyWidth, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.horizontal, screenHPad)
        }
    }
}

/// Reads the actual navigation viewport and pins the full page to that exact
/// width. `containerRelativeFrame` can instead read the underlying tab
/// container on iPhone, which is wider than a pushed navigation destination.
struct DetailViewportFrame<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        GeometryReader { viewport in
            ScrollView {
                content
                    .environment(\.detailViewportWidth, viewport.size.width)
                    .frame(width: viewport.size.width, alignment: .leading)
            }
            .frame(width: viewport.size.width, height: viewport.size.height)
        }
    }
}

#if os(tvOS)
enum TVSeriesDetailMetrics {
    static let headerHeight: CGFloat = 460
    static let posterWidth: CGFloat = 250
    static let posterHeight: CGFloat = 375
}

/// The playable detail page is a single cinematic composition, not a hero
/// image followed by a narrow article column. Keep its essential actions in
/// the first television viewport while leaving enough room for readable copy.
enum TVPlayableDetailMetrics {
    static let heroHeight: CGFloat = 690
    static let copyWidth: CGFloat = 980
    static let bottomInset: CGFloat = 26
}
#endif

struct DetailView: View {
    #if os(tvOS)
    private enum TVDetailFocus: Hashable { case primaryAction }
    @Environment(\.tvShowHome) private var showHome
    #endif

    @EnvironmentObject var model: AppModel
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    let itemId: Int
    @State private var detail: ItemDetail?
    @State private var play: PlayContext?
    @State private var loadError: String?
    @State private var watchBusy = false
    @State private var actionError: String?
    #if os(tvOS)
    @State private var seriesPlayback: PlayContext?
    @FocusState private var tvFocusedAction: TVDetailFocus?
    #endif

    var body: some View {
        DetailViewportFrame {
            Group {
                if let detail {
                    content(detail)
                } else if let loadError {
                    ContentUnavailableView(
                        "Couldn't load this title",
                        systemImage: "exclamationmark.triangle",
                        description: Text(loadError)
                    )
                    .frame(maxWidth: .infinity).padding(.top, 80)
                } else {
                    ProgressView().tint(Palette.accent)
                        .frame(maxWidth: .infinity).padding(.top, 80)
                }
            }
        }
        .background(Palette.bg.ignoresSafeArea())
        #if os(tvOS)
        .safeAreaInset(edge: .top, spacing: 0) {
            tvHomeNavigation
        }
        #endif
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .task(id: itemId) {
            do {
                let loaded = try await model.itemDetail(itemId)
                detail = loaded
                loadError = nil
                #if os(tvOS)
                tvFocusedAction = nil
                if loaded.item.kind == "show" || loaded.item.kind == "season" {
                    seriesPlayback = await model.seriesPlayback(loaded)
                } else {
                    seriesPlayback = nil
                }
                if Self.hasTVPrimaryAction(loaded, seriesPlayback: seriesPlayback) {
                    try? await Task.sleep(for: .milliseconds(120))
                    tvFocusedAction = .primaryAction
                }
                #endif
            } catch {
                loadError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            }
        }
        .fullScreenCover(item: $play) { ctx in
            PlayerView(itemId: ctx.itemId, fileId: ctx.fileId, startMs: ctx.startMs,
                       durationMs: ctx.durationMs, title: ctx.title,
                       subtitle: ctx.subtitle, year: ctx.year, overview: ctx.overview,
                       onStop: { play = nil },
                       onPlayNext: { play = $0 },
                       onPlaybackStopped: { positionMs in
                           updateVisibleProgress(
                               itemId: ctx.itemId,
                               positionMs: positionMs,
                               durationMs: ctx.durationMs
                           )
                       })
                .id(ctx.id)
                .environmentObject(model)
        }
    }

    private func updateVisibleProgress(itemId: Int, positionMs: Int, durationMs: Int) {
        guard let detail else { return }
        self.detail = Self.detail(
            detail,
            applyingPositionMs: positionMs,
            durationMs: durationMs,
            forItemId: itemId
        )
    }

    /// Apply the final player position to the detail snapshot that remains on
    /// screen underneath the full-screen cover. The server receives the same
    /// position from `PlayerController.stop()`; this local copy removes the UI
    /// race without making dismissal wait on the network.
    static func detail(
        _ detail: ItemDetail,
        applyingPositionMs positionMs: Int,
        durationMs: Int,
        forItemId itemId: Int
    ) -> ItemDetail {
        guard detail.item.id == itemId, positionMs > 0 else { return detail }
        var item = detail.item
        var watch = item.watch ?? Watch()
        watch.positionMs = positionMs
        if durationMs > 0 {
            watch.durationMs = durationMs
            if Double(positionMs) >= Double(durationMs) * 0.95 {
                watch.watched = true
            }
        }
        item.watch = watch
        return ItemDetail(
            item: item,
            files: detail.files,
            children: detail.children,
            ancestors: detail.ancestors
        )
    }

    @ViewBuilder
    private func content(_ detail: ItemDetail) -> some View {
        #if os(tvOS)
        if detail.item.kind == "show" || detail.item.kind == "season" {
            tvSeriesContent(detail)
        } else {
            tvPlayableContent(detail)
        }
        #else
        standardContent(detail)
        #endif
    }

    private func standardContent(_ detail: ItemDetail) -> some View {
        let item = detail.item
        let ancestors = detail.ancestors ?? []
        let file = detail.files?.first
        let durationMs = file?.durationMs ?? item.runtimeMs
        let resumeMs = item.watch?.positionMs ?? 0
        let nearlyDone = (durationMs ?? 0) > 0 && Double(resumeMs) > Double(durationMs!) * 0.95
        let canResume = resumeMs > 3000 && !nearlyDone

        return VStack(alignment: .leading, spacing: 0) {
            ZStack(alignment: .bottom) {
                AuthImage(path: item.backdrop ?? item.poster)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .clipped()
                LinearGradient(
                    colors: [.clear, Palette.bg],
                    startPoint: .top, endPoint: .bottom
                )
            }
            .frame(maxWidth: .infinity)
            .frame(height: heroHeight)
            .clipped()

            DetailBodyFrame {
                VStack(alignment: .leading, spacing: 12) {
                    if !ancestors.isEmpty {
                        DetailBreadcrumb(ancestors: ancestors)
                    }

                    Text(item.title)
                        #if os(tvOS)
                        .font(.system(size: 54, weight: .bold))
                        #else
                        .font(.largeTitle.bold())
                        #endif
                        .foregroundColor(Palette.onBg)
                        .lineLimit(nil)
                        .multilineTextAlignment(.leading)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    ItemMetadataBadgeRow(badges: Self.itemMetadataBadges(
                        item,
                        file: file,
                        durationMs: durationMs,
                        includeSeries: ancestors.isEmpty
                    ))

                    if let file, item.isPlayable {
                        playbackActions(
                            item: item,
                            file: file,
                            durationMs: durationMs ?? 0,
                            resumeMs: resumeMs,
                            canResume: canResume
                        )
                        .padding(.top, 4)
                    }

                    watchButton(detail)

                    if let actionError {
                        Text(actionError)
                            .font(.caption)
                            .foregroundColor(Palette.accent)
                    }

                    if let overview = item.overview, !overview.isEmpty {
                        Text(overview)
                            .font(.body)
                            .foregroundColor(Palette.onBg.opacity(0.78))
                            .lineSpacing(4)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.top, 8)
                    }
                }
            }
            .padding(.top, 8)

            if let children = detail.children, !children.isEmpty {
                MediaRow(title: childrenHeading(item.kind), items: children)
                    .padding(.top, 14)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, 30)
    }

    #if os(tvOS)
    private var tvHomeNavigation: some View {
        HStack {
            Button(action: showHome) {
                Label("Home", systemImage: "house.fill")
                    .font(.system(.body, design: .monospaced).weight(.semibold))
            }
            .buttonStyle(TVReadableButtonStyle(prominent: false))
            .focusEffectDisabled()
            .accessibilityHint("Return to the main Home screen")

            Spacer(minLength: 0)
        }
        .padding(.horizontal, 64)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Palette.bg.opacity(0.94))
        .tvNavigationFocusSection()
    }

    private func tvPlayableContent(_ detail: ItemDetail) -> some View {
        let item = detail.item
        let ancestors = detail.ancestors ?? []
        let file = detail.files?.first
        let durationMs = file?.durationMs ?? item.runtimeMs
        let resumeMs = item.watch?.positionMs ?? 0
        let nearlyDone = (durationMs ?? 0) > 0 && Double(resumeMs) > Double(durationMs!) * 0.95
        let canResume = resumeMs > 3000 && !nearlyDone

        return VStack(alignment: .leading, spacing: 0) {
            ZStack(alignment: .bottomLeading) {
                tvPlayableBackground(item)

                VStack(alignment: .leading, spacing: 12) {
                    if !ancestors.isEmpty {
                        DetailBreadcrumb(ancestors: ancestors)
                    }

                    if item.kind != "episode" || ancestors.isEmpty {
                        Text(tvPlayableEyebrow(detail).uppercased())
                            .font(.system(size: 20, weight: .bold, design: .rounded))
                            .tracking(2.6)
                            .foregroundColor(Palette.accent)
                    }

                    Text(item.title)
                        .font(.system(size: 60, weight: .heavy, design: .rounded))
                        .foregroundColor(Palette.onBg)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                        .shadow(color: .black.opacity(0.72), radius: 14, y: 5)

                    tvMetadataLine(item, file: file, durationMs: durationMs)

                    if let overview = item.overview, !overview.isEmpty {
                        Text(overview)
                            .font(.system(size: 24, weight: .regular, design: .rounded))
                            .foregroundColor(Palette.onBg.opacity(0.88))
                            .lineSpacing(3)
                            .lineLimit(2)
                            .fixedSize(horizontal: false, vertical: true)
                    }

                    HStack(spacing: 16) {
                        if let file, item.isPlayable {
                            resumeButton(
                                item: item,
                                file: file,
                                durationMs: durationMs ?? 0,
                                resumeMs: resumeMs,
                                canResume: canResume
                            )
                            .fixedSize()

                            if canResume {
                                startOverButton(item: item, file: file, durationMs: durationMs ?? 0)
                                    .fixedSize()
                            }
                        }

                        watchButton(detail)
                    }
                    .padding(.top, 3)

                    if let actionError {
                        Text(actionError)
                            .font(.caption)
                            .foregroundColor(Palette.accent)
                    }
                }
                .frame(maxWidth: TVPlayableDetailMetrics.copyWidth, alignment: .leading)
                .padding(.leading, 88)
                .padding(.trailing, 70)
                .padding(.bottom, TVPlayableDetailMetrics.bottomInset)
            }
            .frame(height: TVPlayableDetailMetrics.heroHeight)
            .clipped()
            .containerRelativeFrame(.vertical, alignment: .center)
            .tvNavigationFocusSection()

            if let children = detail.children, !children.isEmpty {
                MediaRow(title: childrenHeading(item.kind), items: children)
                    .padding(.top, 8)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, 30)
    }

    private func tvMetadataLine(_ item: Item, file: MediaFile?, durationMs: Int?) -> some View {
        ItemMetadataBadgeRow(badges: Self.itemMetadataBadges(
            item,
            file: file,
            durationMs: durationMs,
            includeSeries: false
        ))
    }

    @ViewBuilder
    private func tvPlayableBackground(_ item: Item) -> some View {
        GeometryReader { geometry in
            HStack(spacing: 0) {
                Spacer(minLength: geometry.size.width * 0.28)
                AuthImage(path: item.backdrop ?? item.poster, contentMode: .fit)
                    .frame(
                        width: geometry.size.width * 0.72,
                        height: geometry.size.height,
                        alignment: .trailing
                    )
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
        }

        LinearGradient(
            stops: [
                .init(color: Palette.bg, location: 0),
                .init(color: Palette.bg.opacity(0.94), location: 0.3),
                .init(color: Palette.bg.opacity(0.38), location: 0.53),
                .init(color: .clear, location: 1)
            ],
            startPoint: .leading,
            endPoint: .trailing
        )

        LinearGradient(
            stops: [
                .init(color: Palette.bg.opacity(0.16), location: 0),
                .init(color: .clear, location: 0.46),
                .init(color: Palette.bg.opacity(0.98), location: 1)
            ],
            startPoint: .top,
            endPoint: .bottom
        )
    }

    private func tvPlayableEyebrow(_ detail: ItemDetail) -> String {
        let item = detail.item
        if item.kind == "episode" {
            return item.showTitle
                ?? detail.ancestors?.last(where: { $0.kind == "show" })?.title
                ?? "TV episode"
        }
        if item.kind == "movie" { return "Movie" }
        return item.kind
    }

    static func tvPlayableMetadata(_ item: Item, file: MediaFile?, durationMs: Int?) -> String {
        tvPlayableMetadataParts(item, file: file, durationMs: durationMs)
            .joined(separator: "   ·   ")
    }

    static func tvPlayableMetadataParts(
        _ item: Item,
        file: MediaFile?,
        durationMs: Int?
    ) -> [String] {
        var parts: [String] = []

        if item.kind == "episode", let season = item.seasonNumber, let episode = item.episodeNumber {
            parts.append("Season \(season), Episode \(episode)")
        }
        if item.kind != "episode", let year = item.year { parts.append(String(year)) }
        if let durationMs, durationMs > 0 { parts.append(tvRuntimeLabel(durationMs)) }
        if let resolution = file.flatMap({
            resolutionLabel(width: $0.width, height: $0.height)
        }) ?? resolutionLabel(item.resolution) {
            parts.append(resolution)
        }
        if let codec = file?.videoCodec, !codec.isEmpty {
            parts.append(tvCodecLabel(codec))
        }
        return parts
    }

    private func tvSeriesContent(_ detail: ItemDetail) -> some View {
        let item = detail.item
        let ancestors = detail.ancestors ?? []
        let children = detail.children ?? []

        return VStack(alignment: .leading, spacing: 0) {
            ZStack {
                tvSeriesBackground(item)

                HStack(alignment: .top, spacing: 46) {
                    AuthImage(path: item.poster ?? item.backdrop)
                        .frame(
                            width: TVSeriesDetailMetrics.posterWidth,
                            height: TVSeriesDetailMetrics.posterHeight
                        )
                        .clipped()
                        .background(Palette.surfaceHi)
                        .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
                        .overlay {
                            RoundedRectangle(cornerRadius: 18, style: .continuous)
                                .stroke(.white.opacity(0.14), lineWidth: 1)
                        }
                        .shadow(color: .black.opacity(0.55), radius: 24, y: 14)

                    VStack(alignment: .leading, spacing: 15) {
                        if !ancestors.isEmpty {
                            DetailBreadcrumb(ancestors: ancestors)
                        } else if let eyebrow = tvSeriesEyebrow(detail) {
                            Text(eyebrow.uppercased())
                                .font(.system(.caption, design: .monospaced).weight(.semibold))
                                .foregroundColor(Palette.accent)
                        }

                        Text(item.title)
                            .font(.system(size: 52, weight: .bold))
                            .foregroundColor(Palette.onBg)
                            .lineLimit(2)
                            .fixedSize(horizontal: false, vertical: true)

                        Text(tvSeriesMeta(item, childCount: children.count))
                            .font(.system(.callout, design: .monospaced))
                            .foregroundColor(Palette.muted)
                            .lineLimit(2)

                        if let overview = item.overview, !overview.isEmpty {
                            Text(overview)
                                .font(.body)
                                .foregroundColor(Palette.onBg.opacity(0.82))
                                .lineSpacing(5)
                                .lineLimit(5)
                                .fixedSize(horizontal: false, vertical: true)
                        }

                        HStack(spacing: 16) {
                            if let seriesPlayback {
                                seriesPlaybackButton(seriesPlayback)
                                    .fixedSize()
                            }
                            watchButton(detail)
                        }

                        if let actionError {
                            Text(actionError)
                                .font(.caption)
                                .foregroundColor(Palette.accent)
                        }
                    }
                    .frame(maxWidth: 980, alignment: .leading)

                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 92)
                .padding(.vertical, 27)
            }
            .frame(height: TVSeriesDetailMetrics.headerHeight)
            .clipped()
            .tvNavigationFocusSection()

            if !children.isEmpty {
                MediaRow(
                    title: childrenHeading(item.kind),
                    items: children,
                    style: Self.tvSeriesChildStyle(for: item.kind)
                )
                .padding(.top, 4)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, 30)
    }

    @ViewBuilder
    private func tvSeriesBackground(_ item: Item) -> some View {
        if let backdrop = item.backdrop {
            AuthImage(path: backdrop)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()
                .opacity(0.3)
        } else {
            Palette.surface
        }

        LinearGradient(
            colors: [.black.opacity(0.2), Palette.bg.opacity(0.72), Palette.bg],
            startPoint: .leading,
            endPoint: .trailing
        )
        LinearGradient(
            colors: [.clear, Palette.bg],
            startPoint: .top,
            endPoint: .bottom
        )
    }

    private func tvSeriesEyebrow(_ detail: ItemDetail) -> String? {
        if detail.item.kind == "season" {
            return detail.ancestors?.last(where: { $0.kind == "show" })?.title ?? "TV season"
        }
        return "TV series"
    }

    private func tvSeriesMeta(_ item: Item, childCount: Int) -> String {
        var parts: [String] = []
        if let year = item.year { parts.append(String(year)) }

        let childName = item.kind == "season" ? "episode" : "season"
        parts.append("\(childCount) \(childName)\(childCount == 1 ? "" : "s")")

        if let rollup = item.rollup, rollup.leaves > 0 {
            if rollup.watched == rollup.leaves {
                parts.append("All \(rollup.leaves) watched")
            } else {
                parts.append("\(rollup.watched) of \(rollup.leaves) watched")
            }
        }
        return parts.joined(separator: "   ·   ")
    }

    static func tvSeriesChildStyle(for kind: String) -> MediaRowStyle {
        kind == "season" ? .episode : .poster
    }

    static func hasTVPrimaryAction(
        _ detail: ItemDetail,
        seriesPlayback: PlayContext?
    ) -> Bool {
        if detail.item.kind == "show" || detail.item.kind == "season" {
            return seriesPlayback != nil
        }
        return detail.item.isPlayable && detail.files?.first != nil
    }

    private func seriesPlaybackButton(_ target: PlayContext) -> some View {
        PrimaryButton(
            title: target.startMs > 0
                ? "▶  Resume · \(formatTime(target.startMs))"
                : "▶  Play"
        ) {
            play = target
        }
        .focused($tvFocusedAction, equals: .primaryAction)
    }
    #endif

    static func itemMetadataBadges(
        _ item: Item,
        file: MediaFile?,
        durationMs: Int?,
        includeSeries: Bool
    ) -> [ItemMetadataBadge] {
        var badges: [ItemMetadataBadge] = []

        if includeSeries,
           item.kind == "episode",
           let showTitle = item.showTitle,
           !showTitle.isEmpty {
            badges.append(ItemMetadataBadge(
                kind: .series,
                symbol: "tv.fill",
                mark: showTitle,
                accessibilityLabel: showTitle
            ))
        }
        if item.kind == "episode", let season = item.seasonNumber, let episode = item.episodeNumber {
            badges.append(ItemMetadataBadge(
                kind: .episode,
                symbol: "rectangle.stack.fill",
                mark: "S\(season) E\(episode)",
                accessibilityLabel: "Season \(season), Episode \(episode)"
            ))
        }
        if item.kind != "episode", let year = item.year {
            badges.append(ItemMetadataBadge(
                kind: .year,
                symbol: "calendar",
                mark: String(year),
                accessibilityLabel: String(year)
            ))
        }
        if let durationMs, durationMs > 0 {
            let runtime = tvRuntimeLabel(durationMs)
            badges.append(ItemMetadataBadge(
                kind: .runtime,
                symbol: "clock.fill",
                mark: runtime,
                accessibilityLabel: runtime
            ))
        }
        if let resolution = file.flatMap({
            resolutionLabel(width: $0.width, height: $0.height)
        }) ?? resolutionLabel(item.resolution) {
            badges.append(ItemMetadataBadge(
                kind: .resolution,
                symbol: resolution == "4K" ? "4k.tv.fill" : "tv.fill",
                mark: resolution == "4K" ? nil : resolution.uppercased(),
                accessibilityLabel: resolution
            ))
        }
        if let codec = file?.videoCodec, !codec.isEmpty {
            let label = tvCodecLabel(codec)
            badges.append(ItemMetadataBadge(
                kind: .video,
                symbol: "film.fill",
                mark: label,
                accessibilityLabel: label
            ))
        }
        return badges
    }

    private static func tvRuntimeLabel(_ durationMs: Int) -> String {
        let totalMinutes = max(1, durationMs / 60_000)
        let hours = totalMinutes / 60
        let minutes = totalMinutes % 60
        if hours == 0 { return "\(minutes) min" }
        return minutes == 0 ? "\(hours) hr" : "\(hours) hr \(minutes) min"
    }

    private static func tvCodecLabel(_ codec: String) -> String {
        switch codec.lowercased().replacingOccurrences(of: "-", with: "") {
        case "h264", "avc": return "H.264"
        case "h265", "hevc": return "HEVC"
        case "av1": return "AV1"
        default: return codec.uppercased()
        }
    }

    private func watchButton(_ detail: ItemDetail) -> some View {
        let watched = isWatched(detail.item)
        return Button {
            Task { await toggleWatched(detail.item, watched: watched) }
        } label: {
            Label(
                watched ? "Mark unwatched" : "Mark watched",
                systemImage: watched ? "checkmark.circle.fill" : "checkmark.circle"
            )
            .font(.system(.body, design: .monospaced))
        }
        #if os(tvOS)
        .buttonStyle(TVReadableButtonStyle(prominent: watched))
        .fixedSize()
        #else
        .buttonStyle(.bordered)
        .tint(watched ? Palette.accent : Palette.muted)
        #endif
        .disabled(watchBusy)
    }

    private func isWatched(_ item: Item) -> Bool {
        if let rollup = item.rollup, rollup.leaves > 0 {
            return rollup.watched >= rollup.leaves
        }
        return item.watch?.watched == true
    }

    private func toggleWatched(_ item: Item, watched: Bool) async {
        watchBusy = true
        actionError = nil
        do {
            try await model.setWatched(itemId: item.id, watched: !watched)
            detail = try await model.itemDetail(item.id)
            await model.loadHome()
        } catch {
            actionError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        }
        watchBusy = false
    }

    private var heroHeight: CGFloat {
        #if os(tvOS)
        return 520
        #else
        return horizontalSizeClass == .regular ? 430 : 270
        #endif
    }

    @ViewBuilder
    private func playbackActions(
        item: Item,
        file: MediaFile,
        durationMs: Int,
        resumeMs: Int,
        canResume: Bool
    ) -> some View {
        #if os(tvOS)
        HStack(spacing: 14) {
            resumeButton(item: item, file: file, durationMs: durationMs,
                         resumeMs: resumeMs, canResume: canResume)
                .fixedSize()
            if canResume {
                startOverButton(item: item, file: file, durationMs: durationMs)
            }
        }
        #else
        let actionLayout = horizontalSizeClass == .compact
            ? AnyLayout(VStackLayout(spacing: 10))
            : AnyLayout(HStackLayout(spacing: 14))

        actionLayout {
            resumeButton(item: item, file: file, durationMs: durationMs,
                         resumeMs: resumeMs, canResume: canResume)
            if canResume {
                startOverButton(item: item, file: file, durationMs: durationMs)
            }
        }
        .frame(maxWidth: .infinity)
        #endif
    }

    @ViewBuilder
    private func resumeButton(
        item: Item,
        file: MediaFile,
        durationMs: Int,
        resumeMs: Int,
        canResume: Bool
    ) -> some View {
        let button = PrimaryButton(title: canResume ? "▶  Resume · \(formatTime(resumeMs))" : "▶  Play") {
            play = PlayContext(
                itemId: item.id,
                fileId: file.id,
                startMs: canResume ? resumeMs : 0,
                durationMs: durationMs,
                title: item.title,
                subtitle: playbackSubtitle(item),
                year: item.year,
                overview: item.overview
            )
        }
        #if os(tvOS)
        button.focused($tvFocusedAction, equals: .primaryAction)
        #else
        button
        #endif
    }

    private func startOverButton(item: Item, file: MediaFile, durationMs: Int) -> some View {
        Button {
            play = PlayContext(
                itemId: item.id,
                fileId: file.id,
                startMs: 0,
                durationMs: durationMs,
                title: item.title,
                subtitle: playbackSubtitle(item),
                year: item.year,
                overview: item.overview
            )
        } label: {
            Text("Start over")
                .font(.system(.body, design: .monospaced))
        }
        #if os(tvOS)
        .buttonStyle(TVReadableButtonStyle(prominent: false))
        #else
        .buttonStyle(IOSFullWidthActionButtonStyle(prominent: false))
        #endif
    }

    private func metaLine(_ item: Item, durationMs: Int?) -> String {
        var parts: [String] = []
        if item.kind == "episode" {
            if let show = item.showTitle { parts.append(show) }
            if let s = item.seasonNumber, let e = item.episodeNumber { parts.append("S\(s) · E\(e)") }
        }
        if let y = item.year { parts.append(String(y)) }
        if let d = durationMs, d > 0 { parts.append(formatTime(d)) }
        return parts.joined(separator: "   ·   ")
    }

    private func playbackSubtitle(_ item: Item) -> String? {
        guard item.kind == "episode" else { return nil }
        var parts: [String] = []
        if let show = item.showTitle, !show.isEmpty { parts.append(show) }
        if let season = item.seasonNumber, let episode = item.episodeNumber {
            parts.append("S\(season) E\(episode)")
        }
        return parts.isEmpty ? nil : parts.joined(separator: "  ·  ")
    }

    private func childrenHeading(_ kind: String) -> String {
        switch kind {
        case "show": return "Seasons"
        case "season": return "Episodes"
        default: return "Contents"
        }
    }
}
