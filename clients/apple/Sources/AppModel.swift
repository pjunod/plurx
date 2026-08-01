import Combine
import Foundation

enum Phase {
    case loading      // checking a saved session on launch
    case needServer   // no server yet, or the saved one is gone
    case needLogin    // server reachable, needs credentials
    case ready        // authenticated
}

/// Single source of truth for the app: session lifecycle (silent reconnect,
/// connect, login, logout) plus the home hubs/libraries and async loaders the
/// screens call. Setting `Session.shared` here changes auth for every following
/// request, image, and media URL at once.
@MainActor
final class AppModel: ObservableObject {
    @Published var phase: Phase = .loading
    @Published var busy = false
    @Published var authError: String?

    @Published var hubs = Hubs()
    @Published var comingSoon: [ComingSoonEntry] = []
    @Published var libraries: [Library] = []
    @Published var libraryPreviews: [Int: [Item]] = [:]
    @Published var homeLoading = true
    @Published var homeError: String?
    @Published var libraryGrouping: LibraryGrouping

    @Published var audioLang: String
    @Published var subLang: String
    @Published var autoplay: Bool
    @Published var posterSize: PosterSize

    /// Starts as the app launches so iOS asks for local-network access before
    /// a person reaches Connect or Sign in. ConnectView observes this object
    /// directly so browser changes redraw without being relayed through the
    /// rest of AppModel.
    let discovery: ServerDiscovery

    private(set) var origin: String
    private(set) var username: String?
    private(set) var serverName: String?

    private let settings = SettingsStore()
    private var api: PlurxAPI?
    private var homeLoadTask: Task<Void, Never>?

    init() {
        discovery = ServerDiscovery()
        origin = settings.origin
        username = settings.username
        audioLang = settings.audioLang
        subLang = settings.subLang
        autoplay = settings.autoplay
        posterSize = settings.posterSize
        libraryGrouping = settings.libraryGrouping
        discovery.start()
        Task {
            // Give NWBrowser a turn to enter its permission-gated operation
            // before a saved-session request starts.
            await Task.yield()
            await bootstrap()
        }
    }

    func requireAPI() -> PlurxAPI {
        if let api { return api }
        let a = PlurxAPI(origin: origin)
        api = a
        return a
    }

    func caps() -> [URLQueryItem] { Caps.query() }

    // MARK: - Session lifecycle

    private func bootstrap() async {
        let savedOrigin = settings.origin
        let savedToken = settings.token
        guard !savedOrigin.isEmpty else { phase = .needServer; return }

        Session.shared.origin = savedOrigin
        origin = savedOrigin
        api = PlurxAPI(origin: savedOrigin)

        guard let savedToken else { phase = .needLogin; return }
        Session.shared.token = savedToken
        do {
            username = try await requireAPI().me().username
            phase = .ready
            discovery.stop()
            await loadHome()
        } catch APIError.http(let code) where code == 401 || code == 403 {
            Session.shared.token = nil       // token rotated / server reset
            phase = .needLogin
        } catch {
            // An offline server or denied LAN permission is not an expired
            // token. Keep it, and say this is connectivity rather than making
            // the person re-enter valid credentials under a false diagnosis.
            authError = "Couldn't reach \(serverName ?? origin). Check Local Network access and the server."
            phase = .needLogin
        }
    }

    func connect(_ raw: String) async {
        let normalized = Self.normalizeOrigin(raw)
        guard !normalized.isEmpty else { return }
        authError = nil
        busy = true
        defer { busy = false }

        Session.shared.origin = normalized
        let a = PlurxAPI(origin: normalized)
        do {
            let info = try await a.serverInfo()
            origin = normalized
            api = a
            serverName = info.name
            settings.origin = normalized
            phase = .needLogin
        } catch {
            authError = "Couldn't reach a plurx server at \(normalized)"
        }
    }

    func login(_ user: String, _ pass: String) async {
        authError = nil
        busy = true
        defer { busy = false }
        do {
            let resp = try await requireAPI().login(
                LoginRequest(username: user.trimmingCharacters(in: .whitespaces), password: pass)
            )
            Session.shared.token = resp.token
            username = resp.user.username
            settings.token = resp.token
            settings.username = resp.user.username
            phase = .ready
            discovery.stop()
            await loadHome()
        } catch APIError.http(let code) where code == 401 || code == 403 {
            authError = "Wrong username or password"
        } catch {
            authError = "Couldn't reach \(serverName ?? origin). Check Local Network access and the server."
        }
    }

    func loadHome() async {
        // `.refreshable` belongs to a view task that SwiftUI may cancel while
        // reconstructing the iPad tab/sidebar hierarchy. Run one shared,
        // unstructured refresh so that lifecycle churn cannot cancel the
        // underlying requests, and coalesce overlapping launch/manual loads.
        if let homeLoadTask {
            await homeLoadTask.value
            return
        }

        let task = Task<Void, Never> { [weak self] in
            guard let self else { return }
            await self.performHomeLoad()
        }
        homeLoadTask = task
        await task.value
        homeLoadTask = nil
    }

    private func performHomeLoad() async {
        homeLoading = true
        homeError = nil
        do {
            async let h = requireAPI().hubs()
            async let l = requireAPI().libraries()
            let loadedHubs = try await h
            let loadedLibraries = try await l
            hubs = loadedHubs
            libraries = loadedLibraries
            homeLoading = false

            comingSoon = (try? await requireAPI().comingSoon().entries) ?? []

            // Prime the category/library shelves after the useful first paint.
            // A failed preview must not hide hubs or make the whole home screen
            // look offline.
            var previews: [Int: [Item]] = [:]
            for library in loadedLibraries {
                if let page = try? await requireAPI().libraryItems(
                    library.id,
                    sort: .added,
                    limit: 24
                ) {
                    previews[library.id] = page.items ?? []
                }
            }
            libraryPreviews = previews
        } catch {
            homeError = Self.homeErrorMessage(for: error, hasCachedContent: hasHomeContent)
            homeLoading = false
        }
    }

    private var hasHomeContent: Bool {
        !libraries.isEmpty
            || !(hubs.continueWatching ?? []).isEmpty
            || !(hubs.nextUp ?? []).isEmpty
            || !(hubs.recentlyAdded ?? []).isEmpty
            || !comingSoon.isEmpty
    }

    /// A cancelled refresh should leave the last good Home screen in place.
    /// The same applies to a transient refresh failure when cached content is
    /// available; a fatal empty-state message is reserved for initial loads
    /// that have never produced anything useful.
    nonisolated static func homeErrorMessage(for error: Error, hasCachedContent: Bool) -> String? {
        if error is CancellationError { return nil }
        if let urlError = error as? URLError, urlError.code == .cancelled { return nil }
        guard !hasCachedContent else { return nil }
        return (error as? LocalizedError)?.errorDescription ?? "Failed to load"
    }

    func logout() {
        settings.clearToken()
        Session.shared.token = nil
        hubs = Hubs()
        comingSoon = []
        libraries = []
        libraryPreviews = [:]
        phase = .needLogin
    }

    func changeServer() {
        Session.shared.token = nil
        phase = .needServer
        discovery.restart()
    }

    func setLanguages(audio: String, sub: String) {
        audioLang = audio
        subLang = sub
        settings.audioLang = audio
        settings.subLang = sub
    }

    func setAutoplay(_ enabled: Bool) {
        autoplay = enabled
        settings.autoplay = enabled
    }

    func setLibraryGrouping(_ grouping: LibraryGrouping) {
        libraryGrouping = grouping
        settings.libraryGrouping = grouping
    }

    func setPosterSize(_ size: PosterSize) {
        posterSize = size
        settings.posterSize = size
    }

    // MARK: - Screen loaders

    func libraryCollections(grouping: LibraryGrouping? = nil) -> [LibraryCollection] {
        let grouping = grouping ?? libraryGrouping
        if grouping == .share {
            return libraries.map {
                LibraryCollection(
                    id: "share:\($0.id)",
                    title: $0.name,
                    kind: Self.canonicalLibraryKind($0.kind),
                    libraries: [$0]
                )
            }
        }

        let orderedKinds = ["movie", "show", "home"]
        let grouped = Dictionary(grouping: libraries) { Self.canonicalLibraryKind($0.kind) }
        let kinds = orderedKinds + grouped.keys.filter { !orderedKinds.contains($0) }.sorted()
        return kinds.compactMap { kind in
            guard let shares = grouped[kind], !shares.isEmpty else { return nil }
            let title: String
            switch kind {
            case "movie": title = "Movies"
            case "show": title = "TV Shows"
            case "home": title = "Home Videos"
            default: title = kind.prefix(1).uppercased() + kind.dropFirst()
            }
            return LibraryCollection(
                id: "category:\(kind)",
                title: title,
                kind: kind,
                libraries: shares.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
            )
        }
    }

    func collection(kind: String) -> LibraryCollection? {
        libraryCollections(grouping: .category).first { $0.kind == kind }
    }

    func previewItems(for collection: LibraryCollection) -> [Item] {
        let ids = Set(collection.libraries.map(\.id))
        return libraryPreviews
            .filter { ids.contains($0.key) }
            .flatMap(\.value)
            .sorted(by: Self.addedFirst)
            .prefix(24)
            .map { $0 }
    }

    func libraryName(for item: Item, in collection: LibraryCollection? = nil) -> String? {
        let candidates = collection?.libraries ?? libraries
        guard candidates.count > 1, let id = item.libraryId else { return nil }
        return candidates.first(where: { $0.id == id })?.name
    }

    func libraryItems(_ collection: LibraryCollection, sort: LibrarySort) async throws -> [Item] {
        var merged: [Item] = []
        for library in collection.libraries {
            var offset = 0
            let limit = 200
            while true {
                let page = try await requireAPI().libraryItems(
                    library.id,
                    sort: sort,
                    offset: offset,
                    limit: limit
                )
                let batch = page.items ?? []
                merged.append(contentsOf: batch)
                offset += batch.count
                let total = page.total ?? batch.count
                if batch.isEmpty || offset >= total || batch.count < limit { break }
            }
        }
        return merged.sorted { Self.compare($0, $1, sort: sort) }
    }

    func search(_ query: String) async throws -> [Item] {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }
        return try await requireAPI().search(trimmed).results ?? []
    }

    func itemDetail(_ id: Int) async throws -> ItemDetail {
        try await requireAPI().item(id)
    }

    func setWatched(itemId: Int, watched: Bool) async throws {
        _ = try await requireAPI().setWatched(itemId: itemId, watched: watched)
    }

    /// The web client's next-episode rule: next child in this season, then the
    /// first episode of the next season. Movies and series finales return nil.
    func nextEpisode(after itemId: Int) async -> PlayContext? {
        guard let current = try? await itemDetail(itemId), current.item.kind == "episode" else {
            return nil
        }
        let ancestors = current.ancestors ?? []
        guard let season = ancestors.last else { return nil }
        let show = ancestors.dropLast().last
        var next: Item?

        if let seasonDetail = try? await itemDetail(season.id) {
            let episodes = (seasonDetail.children ?? []).filter { $0.kind == "episode" }
            if let index = episodes.firstIndex(where: { $0.id == itemId }),
               episodes.indices.contains(index + 1) {
                next = episodes[index + 1]
            }
        }

        if next == nil, let show, let showDetail = try? await itemDetail(show.id) {
            let seasons = (showDetail.children ?? []).filter { $0.kind == "season" }
            if let index = seasons.firstIndex(where: { $0.id == season.id }),
               seasons.indices.contains(index + 1),
               let nextSeason = try? await itemDetail(seasons[index + 1].id) {
                next = (nextSeason.children ?? []).first(where: { $0.kind == "episode" })
            }
        }

        guard let next, let detail = try? await itemDetail(next.id), let file = detail.files?.first else {
            return nil
        }
        return PlayContext(
            itemId: next.id,
            fileId: file.id,
            startMs: 0,
            durationMs: file.durationMs ?? next.runtimeMs ?? 0,
            title: next.title
        )
    }

    func decision(fileId: Int) async throws -> Decision {
        try await requireAPI().decision(fileId: fileId, caps: caps())
    }

    func createHlsSession(fileId: Int, body: CreateSessionRequest) async throws -> HlsStart {
        try await requireAPI().createHlsSession(fileId: fileId, body: body)
    }

    func hlsStatus(_ sessionId: String) async throws -> PlaybackSessionStatus {
        try await requireAPI().hlsStatus(sessionId: sessionId)
    }

    /// Best-effort — the stream is over either way.
    func endHlsSession(_ sessionId: String) async {
        await requireAPI().endHlsSession(sessionId)
    }

    /// Best-effort — a dropped progress beat shouldn't surface an error.
    func reportProgress(itemId: Int, positionMs: Int, durationMs: Int?) async {
        try? await requireAPI().progress(itemId: itemId, positionMs: positionMs, durationMs: durationMs)
    }

    nonisolated static func normalizeOrigin(_ raw: String) -> String {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return s }
        let suppliedScheme = s.hasPrefix("http://") || s.hasPrefix("https://")
        if !suppliedScheme { s = "http://" + s }
        guard var components = URLComponents(string: s) else { return s }
        // A bare host or an explicit HTTP URL with no port means the standard
        // plurx/Plex port. HTTPS keeps 443 for reverse-proxy deployments, and
        // an explicit port always wins.
        if components.port == nil && (!suppliedScheme || components.scheme == "http") {
            components.port = PlurxClientDefaults.port
        }
        if components.path == "/" { components.path = "" }
        while components.path.hasSuffix("/") { components.path.removeLast() }
        return components.string ?? s
    }

    nonisolated static func canonicalLibraryKind(_ raw: String) -> String {
        switch raw.lowercased() {
        case "movie", "movies": return "movie"
        case "show", "shows", "tv": return "show"
        case "home", "home_video", "home_videos": return "home"
        default: return raw.lowercased()
        }
    }

    nonisolated static func matches(_ item: Item, filter: WatchFilter) -> Bool {
        let watched = item.watch?.watched == true
        let progress = (item.watch?.positionMs ?? 0) > 3_000 && !watched
        switch filter {
        case .all: return true
        case .unwatched: return !watched && !progress
        case .inProgress: return progress
        case .watched: return watched
        }
    }

    private nonisolated static func compare(_ lhs: Item, _ rhs: Item, sort: LibrarySort) -> Bool {
        let primary: ComparisonResult
        switch sort {
        case .title:
            primary = sortTitle(lhs.title).localizedCaseInsensitiveCompare(sortTitle(rhs.title))
        case .added:
            primary = compareDescending(lhs.addedAt ?? 0, rhs.addedAt ?? 0)
        case .year:
            primary = compareDescending(lhs.year ?? 0, rhs.year ?? 0)
        case .resolution:
            primary = compareDescending(lhs.resolution ?? 0, rhs.resolution ?? 0)
        case .recorded:
            primary = (rhs.recordedAt ?? "").localizedCaseInsensitiveCompare(lhs.recordedAt ?? "")
        }
        if primary != .orderedSame { return primary == .orderedAscending }
        return lhs.title.localizedCaseInsensitiveCompare(rhs.title) == .orderedAscending
    }

    private nonisolated static func addedFirst(_ lhs: Item, _ rhs: Item) -> Bool {
        compare(lhs, rhs, sort: .added)
    }

    private nonisolated static func compareDescending<T: Comparable>(_ lhs: T, _ rhs: T) -> ComparisonResult {
        if lhs == rhs { return .orderedSame }
        return lhs > rhs ? .orderedAscending : .orderedDescending
    }

    private nonisolated static func sortTitle(_ title: String) -> String {
        let lower = title.lowercased()
        for prefix in ["the ", "an ", "a "] where lower.hasPrefix(prefix) {
            return String(title.dropFirst(prefix.count))
        }
        return title
    }
}
