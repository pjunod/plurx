import SwiftUI

extension View {
    /// Makes the full visual band participate in directional focus searches on
    /// tvOS. This prevents focus from getting trapped when neighboring shelves
    /// or headers have different horizontal extents.
    @ViewBuilder
    func tvNavigationFocusSection() -> some View {
        #if os(tvOS)
        self.focusSection()
        #else
        self
        #endif
    }
}

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
                AuthImage(path: item.poster ?? item.backdrop)
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
            let trailingResolution = resolutionLabel(item.resolution)
            if !metadata.isEmpty || trailingResolution != nil {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    if !metadata.isEmpty {
                        Text(metadata)
                            .font(.system(.caption2, design: .rounded).weight(.medium))
                    }
                    Spacer(minLength: 4)
                    if let trailingResolution {
                        Text(trailingResolution)
                            .font(.system(.caption2, design: .monospaced).weight(.bold))
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
                AuthImage(path: item.backdrop ?? item.poster)
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

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ZStack(alignment: .bottomLeading) {
                AuthImage(path: item.backdrop ?? item.poster)
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
            }

            Text(episodeCardTitle(item))
                .font(.callout.weight(.semibold))
                .foregroundColor(Palette.onBg)
                .lineLimit(1)

            Text(episodeCardMeta(item))
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(Palette.muted)
                .lineLimit(1)
        }
        .frame(width: width, alignment: .leading)
    }
}

struct MediaRow: View {
    @EnvironmentObject var model: AppModel
    let title: String
    let items: [Item]
    var style: MediaRowStyle = .poster
    var collection: LibraryCollection?
    var destination: LibraryCollection?
    var landscapeCopyStyle: LandscapeCardCopyStyle = .plain

    var body: some View {
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .firstTextBaseline) {
                    Text(title)
                        .font(.title3.weight(.semibold))
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
                            NavigationLink(value: Route.item(item.id)) {
                                switch style {
                                case .poster:
                                    PosterCard(
                                        item: item,
                                        width: model.posterSize.posterWidth
                                    )
                                case .landscape:
                                    LandscapeCard(
                                        item: item,
                                        width: model.posterSize.landscapeWidth,
                                        reservesEpisodeSubtitleLine: reservesEpisodeSubtitleLine,
                                        copyStyle: landscapeCopyStyle
                                    )
                                case .episode:
                                    EpisodeCard(
                                        item: item,
                                        width: model.posterSize.landscapeWidth
                                    )
                                }
                            }
                            .posterButtonStyle()
                        }
                    }
                    .padding(.horizontal, screenHPad)
                    #if os(tvOS)
                    .padding(.vertical, 18)
                    #endif
                }
            }
            .padding(.vertical, 10)
            .tvNavigationFocusSection()
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

private func episodeCardTitle(_ item: Item) -> String {
    guard let number = item.episodeNumber else { return item.title }
    return "\(number). \(item.title)"
}

private func episodeCardMeta(_ item: Item) -> String {
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
    return parts.isEmpty ? "Episode" : parts.joined(separator: "   ·   ")
}

struct ComingSoonRow: View {
    @EnvironmentObject var model: AppModel
    let entries: [ComingSoonEntry]

    var body: some View {
        if !entries.isEmpty {
            VStack(alignment: .leading, spacing: 12) {
                Text("Coming Soon")
                    .font(.title3.weight(.semibold))
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
            .tvNavigationFocusSection()
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
                AuthImage(path: entry.poster)
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
        return parts.joined(separator: " · ")
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
    return parts.joined(separator: " · ")
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
