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

struct PosterCard: View {
    let item: Item
    var width: CGFloat = shelfPosterWidth
    var source: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
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
                .font(.headline.weight(.semibold))
                #else
                .font(.callout.weight(.semibold))
                #endif
                .foregroundColor(Palette.onBg)
                .lineLimit(1)
            HStack(spacing: 5) {
                Text(mediaSubtitle(item))
                if let source, !source.isEmpty {
                    Text("·")
                    Text(source)
                }
            }
            .font(.system(.caption, design: .monospaced))
            .foregroundColor(Palette.muted)
            .lineLimit(1)
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
            if let label = resolutionLabel(item.resolution) {
                Text(label)
                    .font(.system(.caption2, design: .monospaced).weight(.bold))
                    .foregroundColor(.white)
                    .padding(.horizontal, 6).padding(.vertical, 3)
                    .background(.black.opacity(0.72), in: Capsule())
                    .padding(7)
            }
        }
    }
}

struct LandscapeCard: View {
    let item: Item
    var width: CGFloat = shelfLandscapeWidth
    var source: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            ZStack(alignment: .bottom) {
                AuthImage(path: item.backdrop ?? item.poster)
                    .frame(width: width, height: width * 9 / 16)
                    .clipped()
                    .background(Palette.surfaceHi)
                    .clipShape(RoundedRectangle(cornerRadius: 11, style: .continuous))

                LinearGradient(colors: [.clear, .black.opacity(0.82)], startPoint: .center, endPoint: .bottom)
                    .clipShape(RoundedRectangle(cornerRadius: 11, style: .continuous))

                VStack(alignment: .leading, spacing: 4) {
                    Spacer()
                    Text(item.showTitle ?? item.title)
                        .font(.headline.weight(.semibold))
                        .foregroundColor(.white)
                        .lineLimit(1)
                    HStack {
                        Text(item.showTitle == nil ? mediaSubtitle(item) : episodeSubtitle(item))
                        Spacer()
                        if let remaining = timeRemaining(item) { Text(remaining) }
                    }
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(.white.opacity(0.76))
                }
                .padding(12)

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
            if let source {
                Text(source)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundColor(Palette.muted)
                    .lineLimit(1)
            }
        }
        .frame(width: width, alignment: .leading)
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
                            Text("See all")
                                .font(.system(.caption, design: .monospaced).weight(.semibold))
                        }
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
                                        width: model.posterSize.posterWidth,
                                        source: model.libraryName(for: item, in: collection)
                                    )
                                case .landscape:
                                    LandscapeCard(
                                        item: item,
                                        width: model.posterSize.landscapeWidth,
                                        source: model.libraryName(for: item, in: collection)
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
        }
    }

    private var shelfSpacing: CGFloat {
        #if os(tvOS)
        return 24
        #else
        return 16
        #endif
    }
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

private func episodeSubtitle(_ item: Item) -> String {
    guard let season = item.seasonNumber, let episode = item.episodeNumber else { return item.title }
    return "S\(season) E\(episode) · \(item.title)"
}

private func timeRemaining(_ item: Item) -> String? {
    guard let position = item.watch?.positionMs,
          let duration = item.watch?.durationMs ?? item.runtimeMs,
          duration > position else { return nil }
    let minutes = max(1, (duration - position) / 60_000)
    return "\(minutes)m left"
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
