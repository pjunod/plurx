import SwiftUI

struct HomeView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        #if os(iOS)
        if #available(iOS 18.0, *) {
            iOSTabs.tabViewStyle(.sidebarAdaptable)
        } else {
            iOSTabs
        }
        #else
        // Detail destinations replace the entire tab shell on television.
        // That keeps the Home tab from remaining visibly selected while an
        // episode or movie detail is on screen.
        NavigationStack {
            tvTabs
                .appDestinations()
        }
        #endif
    }

    #if os(iOS)
    private var iOSTabs: some View {
        TabView {
            NavigationStack {
                HomeDashboard()
                    .appDestinations()
            }
            .tabItem { Label("Home", systemImage: "house") }

            NavigationStack {
                if let movies = model.collection(kind: "movie") {
                    LibraryView(collection: movies)
                        .appDestinations()
                } else {
                    EmptyLibraryCategory(title: "Movies")
                }
            }
            .tabItem { Label("Movies", systemImage: "film") }

            NavigationStack {
                if let shows = model.collection(kind: "show") {
                    LibraryView(collection: shows)
                        .appDestinations()
                } else {
                    EmptyLibraryCategory(title: "TV Shows")
                }
            }
            .tabItem { Label("TV", systemImage: "tv") }

            NavigationStack {
                SearchView()
                    .appDestinations()
            }
            .tabItem { Label("Search", systemImage: "magnifyingglass") }

            NavigationStack {
                SettingsView()
            }
            .tabItem { Label("Settings", systemImage: "gearshape") }
        }
        .tint(Palette.accent)
        .task { if model.homeLoading { await model.loadHome() } }
    }
    #else
    private var tvTabs: some View {
        TabView {
            HomeDashboard()
                .tabItem { Label("Home", systemImage: "house") }

            Group {
                if let movies = model.collection(kind: "movie") {
                    LibraryView(collection: movies)
                } else {
                    EmptyLibraryCategory(title: "Movies")
                }
            }
            .tabItem { Label("Movies", systemImage: "film") }

            Group {
                if let shows = model.collection(kind: "show") {
                    LibraryView(collection: shows)
                } else {
                    EmptyLibraryCategory(title: "TV Shows")
                }
            }
            .tabItem { Label("TV", systemImage: "tv") }

            SearchView()
                .tabItem { Label("Search", systemImage: "magnifyingglass") }

            SettingsView()
                .tabItem { Label("Settings", systemImage: "gearshape") }
        }
        .tint(Palette.accent)
        .task { if model.homeLoading { await model.loadHome() } }
    }
    #endif
}

private struct AppDestinations: ViewModifier {
    func body(content: Content) -> some View {
        content.navigationDestination(for: Route.self) { route in
            switch route {
            case .collection(let collection): LibraryView(collection: collection)
            case .item(let id): DetailView(itemId: id)
            }
        }
    }
}

extension View {
    fileprivate func appDestinations() -> some View { modifier(AppDestinations()) }
}

private struct HomeDashboard: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass

    private var featured: Item? {
        (model.hubs.continueWatching ?? []).first
            ?? (model.hubs.nextUp ?? []).first
            ?? (model.hubs.recentlyAdded ?? []).first
    }

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: dashboardSpacing) {
                #if os(iOS)
                homeHeader
                #endif
                if model.homeLoading {
                    ProgressView().tint(Palette.accent)
                        .frame(maxWidth: .infinity).padding(.top, 80)
                } else if let error = model.homeError {
                    ContentUnavailableView(
                        "Server unavailable",
                        systemImage: "exclamationmark.triangle",
                        description: Text(error)
                    )
                } else {
                    homeContent
                }
            }
            .padding(.bottom, 36)
        }
        .background(Palette.bg.ignoresSafeArea())
        #if os(iOS)
        .toolbar(.hidden, for: .navigationBar)
        .refreshable { await model.loadHome() }
        #endif
    }

    private var dashboardSpacing: CGFloat {
        #if os(tvOS)
        return 12
        #else
        return 6
        #endif
    }

    private var homeHeader: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("plurx")
                .font(.system(size: 32, weight: .bold, design: .monospaced))
                .foregroundColor(Palette.accent)
            Spacer()
            if let username = model.username {
                Text(username)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(Palette.muted)
            }
        }
        .padding(.horizontal, screenHPad)
        .padding(.top, 16)
        .padding(.bottom, 8)
    }

    @ViewBuilder
    private var homeContent: some View {
        if HomeLayoutPolicy.usesFeaturedHero, let featured {
            FeaturedHero(item: featured, compact: horizontalSizeClass == .compact)
                #if os(tvOS)
                .padding(.horizontal, screenHPad)
                #endif
                .padding(.bottom, 12)
        }

        MediaRow(
            title: "Continue Watching",
            items: model.hubs.continueWatching ?? [],
            style: .landscape
        )
        MediaRow(
            title: "Next Up",
            items: model.hubs.nextUp ?? [],
            style: .landscape
        )
        MediaRow(title: "Recently Added", items: model.hubs.recentlyAdded ?? [])
        ComingSoonRow(entries: model.comingSoon)

        if !model.libraries.isEmpty {
            libraryHeading
            ForEach(model.libraryCollections()) { collection in
                MediaRow(
                    title: collection.title,
                    items: model.previewItems(for: collection),
                    collection: collection,
                    destination: collection
                )
            }
        }

        if featured == nil && model.libraries.isEmpty {
            ContentUnavailableView(
                "Your library is empty",
                systemImage: "rectangle.stack",
                description: Text("Add a library in the plurx web app, then pull to refresh.")
            )
            .frame(maxWidth: .infinity)
            .padding(.top, 80)
        }
    }

    private var libraryHeading: some View {
        HStack {
            Text("Browse")
                .font(.title3.weight(.semibold))
                .foregroundColor(Palette.onBg)
            Spacer()
            Picker("Group by", selection: Binding(
                get: { model.libraryGrouping },
                set: { model.setLibraryGrouping($0) }
            )) {
                ForEach(LibraryGrouping.allCases) { grouping in
                    Text(grouping.label).tag(grouping)
                }
            }
            #if os(iOS)
            .pickerStyle(.segmented)
            .frame(maxWidth: 280)
            #endif
        }
        .padding(.horizontal, screenHPad)
        .padding(.top, 18)
    }
}

enum HomeLayoutPolicy {
    #if os(tvOS)
    static let usesFeaturedHero = false
    #else
    static let usesFeaturedHero = true
    #endif
}

private struct FeaturedHero: View {
    let item: Item
    let compact: Bool

    var body: some View {
        NavigationLink(value: Route.item(item.id)) {
            ZStack(alignment: .bottomLeading) {
                AuthImage(path: item.backdrop ?? item.poster)
                    .frame(maxWidth: .infinity)
                    .frame(height: heroHeight)
                    .clipped()
                LinearGradient(
                    colors: [.clear, Palette.bg.opacity(0.18), Palette.bg],
                    startPoint: .top,
                    endPoint: .bottom
                )
                VStack(alignment: .leading, spacing: 8) {
                    Text(item.showTitle ?? item.title)
                        #if os(tvOS)
                        .font(.system(size: 52, weight: .bold))
                        #else
                        .font(compact ? .title.bold() : .largeTitle.bold())
                        #endif
                        .foregroundColor(.white)
                        .lineLimit(2)
                    if item.showTitle != nil {
                        Text(episodeSubtitleForHero(item))
                            .font(.headline)
                            .foregroundColor(.white.opacity(0.82))
                            .lineLimit(1)
                    }
                    if !heroMetadata.isEmpty {
                        Text(heroMetadata)
                            .font(.system(.callout, design: .monospaced).weight(.semibold))
                            .foregroundColor(.white.opacity(0.72))
                            .lineLimit(1)
                    }
                    Label(heroAction, systemImage: "play.fill")
                        .font(.system(.headline, design: .monospaced).weight(.bold))
                        .foregroundColor(.white)
                        .padding(.horizontal, 16).padding(.vertical, 10)
                        .background(Palette.accent, in: Capsule())
                }
                .padding(.horizontal, screenHPad)
                .padding(.bottom, 24)
            }
            .clipShape(RoundedRectangle(cornerRadius: heroCornerRadius, style: .continuous))
        }
        .featuredButtonStyle()
        .accessibilityLabel("\(heroAction) \(item.title)")
    }

    private var heroHeight: CGFloat {
        #if os(tvOS)
        return 470
        #else
        return compact ? 290 : 430
        #endif
    }

    private var heroCornerRadius: CGFloat {
        #if os(tvOS)
        return 24
        #else
        return 0
        #endif
    }

    private var heroMetadata: String {
        var parts: [String] = []
        if let year = item.year { parts.append(String(year)) }
        if let runtime = item.runtimeMs, runtime > 0 {
            let totalMinutes = runtime / 60_000
            let hours = totalMinutes / 60
            let minutes = totalMinutes % 60
            parts.append(hours > 0 ? "\(hours)h \(minutes)m" : "\(minutes)m")
        }
        if let resolution = resolutionLabel(item.resolution) { parts.append(resolution) }
        return parts.joined(separator: "   ·   ")
    }

    private var heroAction: String {
        progressFraction(item.watch, runtimeMs: item.runtimeMs) > 0 ? "Resume" : "View details"
    }
}

private struct EmptyLibraryCategory: View {
    let title: String
    var body: some View {
        ContentUnavailableView(
            "No \(title)",
            systemImage: "rectangle.stack",
            description: Text("No matching library shares are configured on this server.")
        )
        .background(Palette.bg.ignoresSafeArea())
        .navigationTitle(title)
    }
}

private func episodeSubtitleForHero(_ item: Item) -> String {
    var parts: [String] = []
    if let season = item.seasonNumber, let episode = item.episodeNumber {
        parts.append("S\(season) E\(episode)")
    }
    parts.append(item.title)
    return parts.joined(separator: " · ")
}
