import SwiftUI

struct HomeView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.scenePhase) private var scenePhase

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
                LibrariesDashboard()
                    .appDestinations()
            }
            .tabItem { Label("Libraries", systemImage: "rectangle.stack") }

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
        .onChange(of: scenePhase) { _, phase in
            // Coming back to a foregrounded app should not show yesterday's
            // Continue Watching. An Apple TV in particular is suspended rather
            // than quit, so before this the tvOS dashboard only ever refreshed
            // by relaunching the app. `loadHome` coalesces overlapping refreshes
            // and no longer raises a spinner over content, so this is free.
            guard phase == .active else { return }
            Task { await model.loadHome() }
        }
    }
    #else
    private var tvTabs: some View {
        TabView {
            HomeDashboard()
                .tabItem { Label("Home", systemImage: "house") }

            LibrariesDashboard()
                .tabItem { Label("Libraries", systemImage: "rectangle.stack") }

            SearchView()
                .tabItem { Label("Search", systemImage: "magnifyingglass") }

            SettingsView()
                .tabItem { Label("Settings", systemImage: "gearshape") }
        }
        .tint(Palette.accent)
        .task { if model.homeLoading { await model.loadHome() } }
        .onChange(of: scenePhase) { _, phase in
            // Coming back to a foregrounded app should not show yesterday's
            // Continue Watching. An Apple TV in particular is suspended rather
            // than quit, so before this the tvOS dashboard only ever refreshed
            // by relaunching the app. `loadHome` coalesces overlapping refreshes
            // and no longer raises a spinner over content, so this is free.
            guard phase == .active else { return }
            Task { await model.loadHome() }
        }
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
            Text("cinema")
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
            items: HomeLayoutPolicy.continueWatchingShelfItems(
                model.hubs.continueWatching ?? []
            ),
            style: .landscape,
            landscapeCopyStyle: HomeLayoutPolicy.continueWatchingCopyStyle
        )
        MediaRow(
            title: "Next Up",
            items: model.hubs.nextUp ?? [],
            style: .landscape
        )
        MediaRow(
            title: "Recently Added",
            items: model.hubs.recentlyAdded ?? []
        )
        ComingSoonRow(entries: model.comingSoon)

        if featured == nil,
           (model.hubs.nextUp ?? []).isEmpty,
           model.comingSoon.isEmpty {
            ContentUnavailableView(
                "Nothing new yet",
                systemImage: "house",
                description: Text("Continue watching, recently added titles, and upcoming releases will appear here.")
            )
            .frame(maxWidth: .infinity)
            .padding(.top, 80)
        }
    }
}

enum HomeLayoutPolicy {
    static let continueWatchingCopyStyle: LandscapeCardCopyStyle = .accentPanel
    static let topLevelTabs = ["Home", "Libraries", "Search", "Settings"]
    static let showsLibraryShelvesOnHome = false

    static func continueWatchingShelfItems(_ items: [Item]) -> [Item] {
        usesFeaturedHero ? Array(items.dropFirst()) : items
    }

    #if os(tvOS)
    static let usesFeaturedHero = false
    #else
    static let usesFeaturedHero = true
    #endif
}

private struct LibrariesDashboard: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 12) {
                if model.homeLoading {
                    ProgressView()
                        .tint(Palette.accent)
                        .frame(maxWidth: .infinity)
                        .padding(.top, 80)
                } else if let error = model.homeError {
                    ContentUnavailableView(
                        "Server unavailable",
                        systemImage: "exclamationmark.triangle",
                        description: Text(error)
                    )
                } else if model.libraries.isEmpty {
                    ContentUnavailableView(
                        "Your library is empty",
                        systemImage: "rectangle.stack",
                        description: Text("Add a library in the Cinema web app, then pull to refresh.")
                    )
                    .frame(maxWidth: .infinity)
                    .padding(.top, 80)
                } else {
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
            }
            .padding(.bottom, 36)
        }
        .background(Palette.bg.ignoresSafeArea())
        #if os(iOS)
        .toolbar(.hidden, for: .navigationBar)
        .refreshable { await model.loadHome() }
        #endif
    }

    private var libraryHeading: some View {
        HStack {
            Text("Libraries")
                #if os(tvOS)
                .font(.title3.weight(.semibold))
                #else
                .font(.headline.weight(.semibold))
                #endif
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

private struct FeaturedHero: View {
    let item: Item
    let compact: Bool

    var body: some View {
        NavigationLink(value: Route.item(item.id)) {
            #if os(iOS)
            if compact {
                compactHero
            } else {
                expansiveHero
            }
            #else
            expansiveHero
            #endif
        }
        .featuredButtonStyle()
        .accessibilityLabel("\(heroAction) \(item.title)")
    }

    #if os(iOS)
    private var compactHero: some View {
        ZStack(alignment: .bottomLeading) {
            AuthImage(
                path: item.backdrop ?? item.poster,
                targetSize: CGSize(
                    width: 430,
                    height: HomeHeroMetrics.compactHeight
                )
            )
            .frame(maxWidth: .infinity)
            .frame(height: HomeHeroMetrics.compactHeight)
            .clipped()

            LinearGradient(
                stops: [
                    .init(color: .clear, location: 0.18),
                    .init(color: .black.opacity(0.24), location: 0.5),
                    .init(color: Palette.bg.opacity(0.92), location: 1)
                ],
                startPoint: .top,
                endPoint: .bottom
            )

            VStack(alignment: .leading, spacing: 7) {
                Text("CONTINUE")
                    .font(.system(size: 10, weight: .bold, design: .rounded))
                    .tracking(1.7)
                    .foregroundStyle(Palette.accent)

                Text(item.showTitle ?? item.title)
                    .font(.system(size: 25, weight: .heavy, design: .rounded))
                    .foregroundStyle(.white)
                    .lineLimit(1)
                    .shadow(color: .black.opacity(0.6), radius: 8, y: 2)

                if item.showTitle != nil {
                    Text(episodeSubtitleForHero(item))
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.white.opacity(0.78))
                        .lineLimit(1)
                }

                HStack(spacing: 12) {
                    ForEach(compactFacts, id: \.self) { fact in
                        Text(fact)
                    }
                    if let resolutionBadge {
                        IOSWebMediaBadge(badge: resolutionBadge)
                    }
                }
                .font(.system(size: 12, weight: .medium, design: .rounded))
                .foregroundStyle(.white.opacity(0.7))

                if heroProgress > 0 {
                    GeometryReader { geometry in
                        ZStack(alignment: .leading) {
                            Capsule().fill(.white.opacity(0.2))
                            Capsule()
                                .fill(Palette.accent)
                                .frame(width: geometry.size.width * heroProgress)
                        }
                    }
                    .frame(height: 3)
                }

                HStack(spacing: 10) {
                    Label(heroAction, systemImage: "play.fill")
                        .font(.system(size: 13, weight: .bold, design: .rounded))
                        .foregroundStyle(.white)
                        .padding(.horizontal, 14)
                        .frame(height: 38)
                        .background(Palette.accent, in: Capsule())

                    if let remaining = continueWatchingTimeRemaining(item) {
                        Text(remaining)
                            .font(.system(size: 11, weight: .medium, design: .rounded))
                            .foregroundStyle(.white.opacity(0.58))
                    }
                }
            }
            .padding(16)
        }
        .frame(height: HomeHeroMetrics.compactHeight)
        .clipShape(RoundedRectangle(cornerRadius: HomeHeroMetrics.cornerRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: HomeHeroMetrics.cornerRadius, style: .continuous)
                .stroke(.white.opacity(0.08), lineWidth: 0.5)
        }
        .padding(.horizontal, screenHPad)
    }

    private var compactFacts: [String] {
        var facts: [String] = []
        if let year = item.year { facts.append(String(year)) }
        if let runtime = item.runtimeMs, runtime > 0 {
            facts.append(DetailView.compactRuntimeLabel(runtime))
        }
        return facts
    }

    private var resolutionBadge: ItemMetadataBadge? {
        guard let resolution = resolutionLabel(item.resolution) else { return nil }
        return ItemMetadataBadge(
            kind: .resolution,
            symbol: "",
            mark: resolution,
            accessibilityLabel: resolution
        )
    }

    private var heroProgress: CGFloat {
        CGFloat(progressFraction(item.watch, runtimeMs: item.runtimeMs))
    }
    #endif

    private var expansiveHero: some View {
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

#if os(iOS)
enum HomeHeroMetrics {
    static let compactHeight: CGFloat = 238
    static let cornerRadius: CGFloat = 18
}
#endif

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
    return parts.joined(separator: "  ")
}
