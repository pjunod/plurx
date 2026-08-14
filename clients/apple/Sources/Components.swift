import SwiftUI

#if os(tvOS)
let screenHPad: CGFloat = 64
private let shelfPosterWidth: CGFloat = 190
private let shelfLandscapeWidth: CGFloat = 360
#else
let screenHPad: CGFloat = 20
private let shelfPosterWidth: CGFloat = 132
private let shelfLandscapeWidth: CGFloat = 250
#endif

enum MediaRowStyle: Equatable {
    case poster
    case landscape
    case episode
}

enum LandscapeCardCopyStyle: Equatable {
    case plain
    case accentPanel
}

enum EpisodeCardRegion: Equatable {
    case artwork
    case copy
}

enum EpisodeCardAction: Equatable {
    case play
    case navigate(Route)

    var destination: Route? {
        guard case let .navigate(route) = self else { return nil }
        return route
    }
}

/// Keep the region-to-action mapping as a pure contract that XCTest can pin;
/// `EpisodeCard` remains responsible for wiring each mapped action to its own
/// SwiftUI control.
func episodeCardAction(for region: EpisodeCardRegion, itemID: Int) -> EpisodeCardAction {
    switch region {
    case .artwork:
        return .play
    case .copy:
        return .navigate(.item(itemID))
    }
}

/// A Siri Remote cannot focus half a card. tvOS therefore keeps one lifted
/// target and makes Select perform the artwork's primary action: playback.
func tvEpisodeCardSelectionAction() -> EpisodeCardAction {
    .play
}

enum LandscapeAccentPanelMetrics {
    static let fillOpacity = 0.055
    static let strokeOpacity = 0.16
    static let strokeWidth: CGFloat = 0.5
}

struct PosterCard: View {
    let item: Item
    var width: CGFloat = shelfPosterWidth

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            ZStack(alignment: .bottomLeading) {
                AuthImage(
                    path: item.poster ?? item.backdrop,
                    targetSize: CGSize(width: width, height: width * 1.5)
                )
                    .frame(width: width, height: width * 1.5)
                    .clipped()
                    .background(Palette.surfaceHi)
                    .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))

                cardBadges

                let fraction = progressFraction(item.watch, runtimeMs: item.runtimeMs)
                if fraction > 0, fraction < 0.98 {
                    GeometryReader { geometry in
                        VStack {
                            Spacer()
                            HStack(spacing: 0) {
                                Rectangle()
                                    .fill(Palette.accent)
                                    .frame(width: geometry.size.width * fraction, height: 4)
                                Rectangle().fill(.white.opacity(0.22)).frame(height: 4)
                            }
                        }
                    }
                }
            }
            .aspectRatio(2 / 3, contentMode: .fit)

            Text(item.title)
                #if os(tvOS)
                .font(.callout.weight(.semibold))
                #else
                .font(.callout.weight(.semibold))
                #endif
                .foregroundColor(Palette.onBg)
                .lineLimit(1)
            let metadata = cardShelfMetadata(item)
            if let episodeBadges = posterCardEpisodeSummaryBadges(item) {
                if !metadata.isEmpty {
                    Text(metadata)
                        .font(.system(.caption2, design: .rounded).weight(.medium))
                        .foregroundColor(Palette.muted)
                        .lineLimit(1)
                }
                EpisodeMediaSummary(badges: episodeBadges)
            } else if !metadata.isEmpty || resolutionBadge(resolutionLabel(item.resolution)) != nil {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    if !metadata.isEmpty {
                        Text(metadata)
                            .font(.system(.caption2, design: .rounded).weight(.medium))
                    }
                    Spacer(minLength: 4)
                    if let trailingResolution = resolutionBadge(resolutionLabel(item.resolution)) {
                        #if os(iOS)
                        IOSWebMediaBadge(badge: trailingResolution)
                        #else
                        Text(trailingResolution.accessibilityLabel)
                            .font(.system(.caption2, design: .monospaced).weight(.bold))
                        #endif
                    }
                }
                .frame(maxWidth: .infinity)
                .foregroundColor(Palette.muted)
                .lineLimit(1)
            }
        }
        .frame(width: width, alignment: .leading)
    }

    @ViewBuilder
    private var cardBadges: some View {
        VStack(alignment: .leading, spacing: 5) {
            if item.watch?.watched == true {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(.white, Palette.accent)
                    .padding(7)
            }
            Spacer()
        }
    }
}

struct LandscapeCard: View {
    let item: Item
    var width: CGFloat = shelfLandscapeWidth
    var reservesEpisodeSubtitleLine = false
    var copyStyle: LandscapeCardCopyStyle = .plain

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            ZStack(alignment: .bottom) {
                AuthImage(
                    path: item.backdrop ?? item.poster,
                    targetSize: CGSize(width: width, height: width * 9 / 16)
                )
                    .frame(width: width, height: width * 9 / 16)
                    .clipped()
                    .background(Palette.surfaceHi)
                    .clipShape(RoundedRectangle(cornerRadius: 11, style: .continuous))

                let fraction = progressFraction(item.watch, runtimeMs: item.runtimeMs)
                if fraction > 0, fraction < 0.98 {
                    GeometryReader { geometry in
                        VStack {
                            Spacer()
                            Rectangle()
                                .fill(Palette.accent)
                                .frame(width: geometry.size.width * fraction, height: 4)
                        }
                    }
                }
            }
            VStack(alignment: .leading, spacing: 5) {
                if copyStyle == .accentPanel {
                    continueWatchingCopy
                } else {
                    standardLandscapeCopy
                }
            }
            .padding(.horizontal, copyStyle == .accentPanel ? 10 : 0)
            .padding(.vertical, copyStyle == .accentPanel ? 7 : 0)
            .frame(
                width: copyStyle == .accentPanel ? width : nil,
                alignment: .leading
            )
            .background {
                if copyStyle == .accentPanel {
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .fill(Palette.accent.opacity(LandscapeAccentPanelMetrics.fillOpacity))
                }
            }
            .overlay {
                if copyStyle == .accentPanel {
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .stroke(
                            Palette.accent.opacity(LandscapeAccentPanelMetrics.strokeOpacity),
                            lineWidth: LandscapeAccentPanelMetrics.strokeWidth
                        )
                }
            }
        }
        .frame(width: width, alignment: .leading)
    }

    @ViewBuilder
    private var continueWatchingCopy: some View {
        Text(landscapeCardShowTitle(item) ?? item.title)
            #if os(tvOS)
            .font(.callout.weight(.semibold))
            #else
            .font(.headline.weight(.semibold))
            #endif
            .foregroundColor(Palette.onBg)
            .lineLimit(1)

        HStack(alignment: .firstTextBaseline, spacing: 8) {
            let detail = continueWatchingDetail(item)
            if !detail.isEmpty {
                Text(detail)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if let remaining = continueWatchingTimeRemaining(item) {
                Text(remaining)
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
                    .multilineTextAlignment(.trailing)
            }
        }
        .font(.system(.caption2, design: .rounded).weight(.medium))
        .foregroundColor(Palette.muted)
    }

    @ViewBuilder
    private var standardLandscapeCopy: some View {
        Text(landscapeCardShowTitle(item) ?? item.title)
            #if os(tvOS)
            .font(.callout.weight(.semibold))
            #else
            .font(.headline.weight(.semibold))
            #endif
            .foregroundColor(Palette.onBg)
            .lineLimit(1)

        if landscapeCardShowTitle(item) != nil {
            Text(item.title)
                .font(.caption)
                .foregroundColor(Palette.muted)
                .lineLimit(1)
        }

        let metadata = cardShelfMetadata(item)
        if !metadata.isEmpty {
            Text(metadata)
                .font(.system(.caption2, design: .rounded).weight(.medium))
                .foregroundColor(Palette.muted)
                .lineLimit(1)
        } else {
            Text("Metadata")
                .font(.system(.caption2, design: .rounded).weight(.medium))
                .hidden()
                .accessibilityHidden(true)
        }

        if landscapeCardShowTitle(item) == nil, reservesEpisodeSubtitleLine {
            Text("Episode title")
                .font(.caption)
                .hidden()
                .accessibilityHidden(true)
        }
    }
}

struct EpisodeCard: View {
    let item: Item
    var width: CGFloat = shelfLandscapeWidth
    var isStarting = false
    let onPlay: () -> Void

    var body: some View {
        #if os(tvOS)
        Button {
            perform(tvEpisodeCardSelectionAction())
        } label: {
            cardContent
        }
        .posterButtonStyle()
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(episodeCardPlayAccessibilityLabel(item))
        .accessibilityValue(tvEpisodeCardAccessibilityValue(item, isStarting: isStarting))
        #else
        VStack(alignment: .leading, spacing: 8) {
            Button {
                perform(episodeCardAction(for: .artwork, itemID: item.id))
            } label: {
                artwork.contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(episodeCardPlayAccessibilityLabel(item))
            .accessibilityValue(episodeCardPlayAccessibilityValue(item, isStarting: isStarting))

            if let destination = episodeCardAction(
                for: .copy,
                itemID: item.id
            ).destination {
                NavigationLink(value: destination) {
                    copy.contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(episodeCardDetailsAccessibilityLabel(item))
                .accessibilityValue(episodeCardDetailsAccessibilityValue(item))
            }
        }
        .frame(width: width, alignment: .leading)
        #endif
    }

    private var cardContent: some View {
        VStack(alignment: .leading, spacing: 8) {
            artwork
            copy
        }
        .frame(width: width, alignment: .leading)
    }

    private var artwork: some View {
        ZStack(alignment: .bottomLeading) {
            AuthImage(
                path: item.backdrop ?? item.poster,
                targetSize: CGSize(width: width, height: width * 9 / 16)
            )
                .frame(width: width, height: width * 9 / 16)
                .clipped()
                .background(Palette.surfaceHi)
                .clipShape(RoundedRectangle(cornerRadius: 11, style: .continuous))

            LinearGradient(
                colors: [.clear, .black.opacity(0.78)],
                startPoint: .center,
                endPoint: .bottom
            )
            .clipShape(RoundedRectangle(cornerRadius: 11, style: .continuous))

            if item.watch?.watched == true {
                Label("Watched", systemImage: "checkmark.circle.fill")
                    .font(.system(.caption2, design: .monospaced).weight(.semibold))
                    .foregroundColor(.white)
                    .padding(.horizontal, 9)
                    .padding(.vertical, 6)
                    .background(.black.opacity(0.72), in: Capsule())
                    .padding(10)
            }

            let fraction = progressFraction(item.watch, runtimeMs: item.runtimeMs)
            if fraction > 0, fraction < 0.98 {
                GeometryReader { geometry in
                    VStack {
                        Spacer()
                        HStack(spacing: 0) {
                            Rectangle()
                                .fill(Palette.accent)
                                .frame(width: geometry.size.width * fraction, height: 5)
                            Rectangle().fill(.white.opacity(0.2)).frame(height: 5)
                        }
                    }
                }
            }

            if isStarting {
                RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .fill(.black.opacity(0.48))
                ProgressView()
                    .tint(.white)
                    .accessibilityHidden(true)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }

    private var copy: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(episodeCardTitle(item))
                .font(.callout.weight(.semibold))
                .foregroundColor(Palette.onBg)
                .lineLimit(1)

            Text(episodeCardMeta(item))
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(Palette.muted)
                .lineLimit(1)

            EpisodeMediaSummary(badges: episodeMediaSummaryBadges(item))
        }
        .frame(width: width, alignment: .leading)
    }

    private func perform(_ action: EpisodeCardAction) {
        switch action {
        case .play:
            onPlay()
        case .navigate:
            assertionFailure("EpisodeCard dispatches navigation through its NavigationLink")
        }
    }
}

struct EpisodeMediaSummary: View {
    let badges: [ItemMetadataBadge]

    @ViewBuilder
    var body: some View {
        if badges.isEmpty {
            Color.clear
                .frame(height: 20)
                .accessibilityHidden(true)
        } else {
            #if os(iOS)
            HStack(spacing: 5) {
                ForEach(badges) { badge in
                    IOSWebMediaBadge(badge: badge)
                }
            }
            .frame(maxWidth: .infinity, minHeight: 20, alignment: .leading)
            .clipped()
            #else
            Text(badges.map { $0.mark ?? $0.accessibilityLabel }.joined(separator: " · "))
                .font(.system(.caption2, design: .monospaced).weight(.bold))
                .foregroundStyle(Palette.muted)
                .lineLimit(1)
                .frame(maxWidth: .infinity, minHeight: 20, alignment: .leading)
                .accessibilityLabel(badges.map(\.accessibilityLabel).joined(separator: ", "))
            #endif
        }
    }
}

/// Season rows intentionally stop at the two facts that distinguish picture
/// quality at a glance. Codec, audio and file details belong on the episode
/// page; repeating them here would turn a compact shelf into a spec table.
func episodeMediaSummaryBadges(_ item: Item) -> [ItemMetadataBadge] {
    var badges: [ItemMetadataBadge] = []
    if let badge = resolutionBadge(resolutionLabel(item.media?.height ?? item.resolution)) {
        badges.append(badge)
    }
    if let range = PlayerView.dynamicRangeBadge(
        hdr: item.media?.hdr,
        hdrFormat: item.media?.hdrFormat,
        delivered: nil,
        displayHDR: true
    ) {
        badges.append(ItemMetadataBadge(
            kind: .dynamicRange,
            symbol: range.symbol,
            mark: range.mark,
            accessibilityLabel: range.accessibilityLabel
        ))
    }
    return badges
}

/// Poster season shelves stay visually compact on iPhone and iPad, but their
/// episode cards still receive the same resolution/HDR pair as tvOS's
/// landscape cards. Returning `nil` for every other kind keeps ordinary
/// poster shelves on their existing one-line metadata layout.
func posterCardEpisodeSummaryBadges(_ item: Item) -> [ItemMetadataBadge]? {
    guard item.kind == "episode" else { return nil }
    return episodeMediaSummaryBadges(item)
}

struct MediaRow: View {
    @EnvironmentObject var model: AppModel
    let title: String
    let items: [Item]
    var style: MediaRowStyle = .poster
    var collection: LibraryCollection?
    var destination: LibraryCollection?
    var landscapeCopyStyle: LandscapeCardCopyStyle = .plain
    var startingEpisodeID: Int?
    var onPlayEpisode: ((Item) -> Void)?

    var body: some View {
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .firstTextBaseline) {
                    Text(title)
                        #if os(tvOS)
                        .font(.title3.weight(.semibold))
                        #else
                        .font(.headline.weight(.semibold))
                        #endif
                        .foregroundColor(Palette.onBg)
                    Spacer()
                    if let destination {
                        NavigationLink(value: Route.collection(destination)) {
                            HStack(spacing: 8) {
                                Text("See All")
                                Image(systemName: "chevron.right")
                                    .font(.caption.weight(.bold))
                            }
                            .font(.system(.caption, design: .rounded).weight(.bold))
                        }
                        .shelfActionButtonStyle()
                    }
                }
                .padding(.horizontal, screenHPad)

                ScrollView(.horizontal, showsIndicators: false) {
                    LazyHStack(alignment: .top, spacing: shelfSpacing) {
                        ForEach(items) { item in
                            switch style {
                            case .poster:
                                NavigationLink(value: Route.item(item.id)) {
                                    PosterCard(
                                        item: item,
                                        width: model.posterSize.posterWidth
                                    )
                                }
                                .posterButtonStyle()
                            case .landscape:
                                NavigationLink(value: Route.item(item.id)) {
                                    LandscapeCard(
                                        item: item,
                                        width: model.posterSize.landscapeWidth,
                                        reservesEpisodeSubtitleLine: reservesEpisodeSubtitleLine,
                                        copyStyle: landscapeCopyStyle
                                    )
                                }
                                .posterButtonStyle()
                            case .episode:
                                EpisodeCard(
                                    item: item,
                                    width: model.posterSize.landscapeWidth,
                                    isStarting: episodeCardIsStarting(
                                        startingEpisodeID: startingEpisodeID,
                                        itemID: item.id
                                    ),
                                    onPlay: { onPlayEpisode?(item) }
                                )
                            }
                        }
                    }
                    .padding(.horizontal, screenHPad)
                    #if os(tvOS)
                    .padding(.vertical, 18)
                    #endif
                }
            }
            .padding(.vertical, 10)
        }
    }

    private var shelfSpacing: CGFloat {
        #if os(tvOS)
        return 24
        #else
        return 16
        #endif
    }

    private var reservesEpisodeSubtitleLine: Bool {
        style == .landscape && landscapeShelfNeedsEpisodeSubtitleLine(items)
    }
}

func landscapeShelfNeedsEpisodeSubtitleLine(_ items: [Item]) -> Bool {
    items.contains { landscapeCardShowTitle($0) != nil }
}

private func landscapeCardShowTitle(_ item: Item) -> String? {
    guard let showTitle = item.showTitle?.trimmingCharacters(in: .whitespacesAndNewlines),
          !showTitle.isEmpty else { return nil }
    return showTitle
}

func episodeCardTitle(_ item: Item) -> String {
    guard let number = item.episodeNumber else { return item.title }
    return "\(number). \(item.title)"
}

func episodeCardPlayAccessibilityLabel(_ item: Item) -> String {
    "Play \(episodeCardTitle(item))"
}

func episodeCardDetailsAccessibilityLabel(_ item: Item) -> String {
    "View details for \(episodeCardTitle(item))"
}

func episodeCardIsStarting(startingEpisodeID: Int?, itemID: Int) -> Bool {
    startingEpisodeID == itemID
}

func episodeCardPlayAccessibilityValue(_ item: Item, isStarting: Bool) -> String {
    if isStarting { return "Starting playback" }
    if item.watch?.watched == true { return "Watched" }
    if let position = item.watch?.positionMs, position > 0 {
        if let remaining = timeRemaining(item) {
            return "In progress, \(remaining)"
        }
        return "In progress"
    }
    return "Unwatched"
}

func episodeCardDetailsAccessibilityValue(_ item: Item) -> String {
    ([episodeCardMeta(item)] + episodeMediaSummaryBadges(item).map(\.accessibilityLabel))
        .joined(separator: ", ")
}

func tvEpisodeCardAccessibilityValue(_ item: Item, isStarting: Bool) -> String {
    [
        episodeCardPlayAccessibilityValue(item, isStarting: isStarting),
        episodeCardDetailsAccessibilityValue(item),
    ].joined(separator: ", ")
}

func episodeCardMeta(_ item: Item) -> String {
    var parts: [String] = []
    if let airDate = item.airDate, !airDate.isEmpty {
        parts.append(String(airDate.prefix(10)))
    }
    if let runtime = item.runtimeMs, runtime > 0 {
        parts.append(formatTime(runtime))
    }
    if let remaining = timeRemaining(item) {
        parts.append(remaining)
    }
    return parts.isEmpty ? "Episode" : parts.joined(separator: "   ")
}

struct ComingSoonRow: View {
    @EnvironmentObject var model: AppModel
    let entries: [ComingSoonEntry]

    var body: some View {
        if !entries.isEmpty {
            VStack(alignment: .leading, spacing: 12) {
                Text("Coming Soon")
                    #if os(tvOS)
                    .font(.title3.weight(.semibold))
                    #else
                    .font(.headline.weight(.semibold))
                    #endif
                    .foregroundColor(Palette.onBg)
                    .padding(.horizontal, screenHPad)
                ScrollView(.horizontal, showsIndicators: false) {
                    LazyHStack(alignment: .top, spacing: comingSoonSpacing) {
                        ForEach(entries) { entry in
                            if let itemId = entry.itemId {
                                NavigationLink(value: Route.item(itemId)) {
                                    ComingSoonCard(entry: entry, width: model.posterSize.posterWidth)
                                }
                                .posterButtonStyle()
                            } else {
                                ComingSoonCard(entry: entry, width: model.posterSize.posterWidth)
                            }
                        }
                    }
                    .padding(.horizontal, screenHPad)
                    #if os(tvOS)
                    .padding(.vertical, 18)
                    #endif
                }
            }
            .padding(.vertical, 10)
        }
    }

    private var comingSoonSpacing: CGFloat {
        #if os(tvOS)
        return 24
        #else
        return 16
        #endif
    }
}

private struct ComingSoonCard: View {
    let entry: ComingSoonEntry
    let width: CGFloat

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            ZStack(alignment: .bottomLeading) {
                AuthImage(
                    path: entry.poster,
                    targetSize: CGSize(width: width, height: width * 1.5)
                )
                    .frame(width: width, height: width * 1.5)
                    .clipped()
                    .background(Palette.surfaceHi)
                    .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                LinearGradient(colors: [.clear, .black.opacity(0.82)], startPoint: .center, endPoint: .bottom)
                    .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                Text(shortDate(entry.date))
                    .font(.system(.caption, design: .monospaced).weight(.bold))
                    .foregroundColor(.white)
                    .padding(8)
            }
            Text(entry.title)
                #if os(tvOS)
                .font(.headline.weight(.semibold))
                #else
                .font(.callout.weight(.semibold))
                #endif
                .foregroundColor(Palette.onBg)
                .lineLimit(1)
            Text(entry.detail.isEmpty ? entry.kind.capitalized : entry.detail)
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(Palette.muted)
                .lineLimit(1)
        }
        .frame(width: width, alignment: .leading)
    }
}

func resolutionLabel(_ height: Int?) -> String? {
    guard let height else { return nil }
    if height >= 2160 { return "4K" }
    if height >= 1080 { return "1080p" }
    if height >= 720 { return "720p" }
    if height > 0 { return "\(height)p" }
    return nil
}

/// Video quality tiers describe the encoded raster rather than blindly using
/// `height`: both 1920×1080 and orientation-ordered 1080×1920 are 1080p, while
/// a cropped 3840×1608 scope encode is still 4K.
func resolutionLabel(width: Int?, height: Int?) -> String? {
    guard let height, height > 0 else { return nil }
    guard let width, width > 0 else { return resolutionLabel(height) }
    let longEdge = max(width, height)
    let shortEdge = min(width, height)
    if longEdge >= 3_200 || shortEdge >= 1_700 { return "4K" }
    if longEdge >= 2_300 || shortEdge >= 1_300 { return "1440p" }
    if longEdge >= 1_600 || shortEdge >= 900 { return "1080p" }
    if longEdge >= 1_100 || shortEdge >= 650 { return "720p" }
    if longEdge >= 700 || shortEdge >= 400 { return "480p" }
    return "\(shortEdge)p"
}

/// One resolution label produces one badge everywhere: detail metadata,
/// tvOS episode cards, and compact iOS/iPadOS poster cards cannot drift in
/// symbol, casing, or accessibility text.
func resolutionBadge(_ resolution: String?) -> ItemMetadataBadge? {
    guard let resolution else { return nil }
    return ItemMetadataBadge(
        kind: .resolution,
        symbol: resolution == "4K" ? "4k.tv.fill" : "tv.fill",
        mark: resolution == "4K" ? nil : resolution.uppercased(),
        accessibilityLabel: resolution
    )
}

private func timeRemaining(_ item: Item) -> String? {
    guard let position = item.watch?.positionMs,
          let duration = item.watch?.durationMs ?? item.runtimeMs,
          duration > position else { return nil }
    let minutes = max(1, (duration - position) / 60_000)
    return "\(minutes)m left"
}

/// Continue Watching uses exactly two visible copy rows. Episodes combine
/// their season/episode number and title on the lower row; movies use the year.
func continueWatchingDetail(_ item: Item) -> String {
    if landscapeCardShowTitle(item) != nil {
        var parts: [String] = []
        if let season = item.seasonNumber, let episode = item.episodeNumber {
            parts.append("S\(season) E\(episode)")
        }
        parts.append(item.title)
        return parts.joined(separator: "  ")
    }
    return item.year.map(String.init) ?? ""
}

func continueWatchingTimeRemaining(_ item: Item) -> String? {
    timeRemaining(item)
}

/// The shelf's lower line should identify the item, not repeat its library
/// category. Episodes carry season/episode, movies carry year, and an active
/// watch carries the genuinely useful remaining time.
func cardShelfMetadata(_ item: Item) -> String {
    var parts: [String] = []
    if let season = item.seasonNumber, let episode = item.episodeNumber {
        parts.append("S\(season) E\(episode)")
    } else if let year = item.year {
        parts.append(String(year))
    }
    if let remaining = timeRemaining(item) {
        parts.append(remaining)
    }
    return parts.joined(separator: "  ")
}

private func shortDate(_ raw: String) -> String {
    let datePart = String(raw.prefix(10))
    let parser = DateFormatter()
    parser.locale = Locale(identifier: "en_US_POSIX")
    parser.dateFormat = "yyyy-MM-dd"
    guard let date = parser.date(from: datePart) else { return datePart }
    let formatter = DateFormatter()
    formatter.setLocalizedDateFormatFromTemplate("MMM d")
    return formatter.string(from: date)
}
