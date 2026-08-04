import Combine
import Foundation

enum Phase {
    case loading      // checking a saved session on launch
    case needServer   // no server yet, or the saved one is gone
    case needLogin    // server reachable, needs credentials
    case reconnectFailed // saved credentials are intact; the server is temporarily unreachable
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
    @Published var theme: ViewerTheme
    @Published var appearance: ViewerAppearance
    @Published var posterSize: PosterSize
    @Published var subtitleReadiness: SubtitleReadiness

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
        theme = settings.theme
        appearance = settings.appearance
        posterSize = settings.posterSize
        subtitleReadiness = settings.subtitleReadiness
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
            await backfillServerIdentityIfNeeded()
            phase = .ready
            discovery.stop()
            await loadHome()
        } catch APIError.http(let code) where code == 401 || code == 403 {
            Session.shared.token = nil       // token rotated / server reset
            settings.clearToken()
            phase = .needLogin
        } catch {
            if let recovered = await rediscoverSavedServer(
                expectedInstanceId: settings.instanceId,
                savedOrigin: savedOrigin
            ) {
                await retrySavedSession(at: recovered, token: savedToken)
            } else {
                showReconnectFailure()
            }
        }
    }

    func retrySavedSession() async {
        authError = nil
        phase = .loading
        await bootstrap()
    }

    func connect(_ raw: String) async {
        let normalized = Self.normalizeOrigin(raw)
        guard !normalized.isEmpty else { return }
        authError = nil
        busy = true
        defer { busy = false }

        // Point the shared session at the candidate before probing it, and drop
        // any in-memory bearer in the same breath: a token belongs to exactly
        // one origin, so not even a failed probe may leave the previous
        // server's credential attached to this address.
        Session.shared.origin = normalized
        Session.shared.token = nil
        let a = PlurxAPI(origin: normalized)
        do {
            let info = try await a.serverInfo()
            origin = normalized
            api = a
            serverName = info.name
            // The persisted half of the same invariant. A relaunch between here
            // and the login below must not be able to hand server A's bearer to
            // server B — one write, both facts.
            settings.setServer(origin: normalized, instanceId: info.instanceId, token: nil)
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
        // The spinner belongs to a dashboard that has never held anything
        // useful. Raising it for every refresh is what let pull-to-refresh —
        // and, now, returning from the player or from the background — blank a
        // populated screen for the length of a round trip. Both dashboards key
        // their `ProgressView` off this flag, so the policy lives here once.
        homeLoading = !hasHomeContent
        homeError = nil
        do {
            // Three independent requests, one round trip. Coming Soon is
            // started with the other two rather than after them: it is never
            // required for first paint, and waiting for it delayed the shelves.
            async let h = requireAPI().hubs()
            async let l = requireAPI().libraries()
            async let soon = requireAPI().comingSoon()
            let loadedHubs = try await h
            let loadedLibraries = try await l
            hubs = loadedHubs
            libraries = loadedLibraries
            // Paint as soon as the responses that make a home screen useful
            // have arrived, not once the last shelf preview has.
            homeLoading = false

            comingSoon = (try? await soon)?.entries ?? []

            // Prime the category/library shelves after the useful first paint,
            // publishing each page the moment it lands so shelves fill in
            // rather than appearing all at once at the end. A failed preview
            // must not hide hubs or make the whole home screen look offline.
            var removed = Set(libraryPreviews.keys)
            for library in loadedLibraries {
                removed.remove(library.id)
                if let page = try? await requireAPI().libraryItems(
                    library.id,
                    sort: .added,
                    limit: 24
                ) {
                    libraryPreviews[library.id] = page.items ?? []
                }
            }
            // Shelves are published incrementally, so a library that has since
            // been deleted server-side has to be dropped explicitly; the old
            // whole-dictionary replacement did it implicitly.
            for id in removed { libraryPreviews.removeValue(forKey: id) }
        } catch {
            noteAuthFailure(error)
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

    /// Is this the answer a server gives to a bearer it no longer honors?
    /// Rotating the signing secret, a server reset, or an administrator
    /// revoking a session all land here.
    nonisolated static func isSessionExpired(_ error: Error) -> Bool {
        guard let api = error as? APIError, case .http(let code) = api else { return false }
        return code == 401 || code == 403
    }

    /// One place where a signed-in session discovers it is no longer signed in.
    /// Bootstrap already handled its own 401/403; every screen after it turned
    /// a rotated token into a per-screen error string instead, so a viewer got
    /// "Server returned 401" on Home, on every library, and on every detail
    /// page while still appearing to be logged in. Returns whether it acted, so
    /// callers can skip an error message that is about to be replaced by the
    /// login screen.
    @discardableResult
    func noteAuthFailure(_ error: Error) -> Bool {
        // Only after bootstrap: the launch paths clear their own token and
        // choose between `.needLogin` and `.reconnectFailed` themselves.
        guard case .ready = phase, Self.isSessionExpired(error) else { return false }
        signOut()
        return true
    }

    func logout() {
        signOut()
    }

    private func signOut() {
        settings.clearToken()
        Session.shared.token = nil
        AuthImageCache.shared.clear()
        hubs = Hubs()
        comingSoon = []
        libraries = []
        libraryPreviews = [:]
        homeLoading = true
        homeError = nil
        phase = .needLogin
    }

    func changeServer() {
        // Clearing the origin clears the persisted bearer in the same write:
        // abandoning a server must not leave its credential on disk where the
        // next one could be handed it, whether or not this process survives to
        // reach the login screen. The in-memory `origin` deliberately stays as
        // the connect screen's manual-entry prefill — it is an address, not a
        // credential, and both copies of the credential are gone.
        settings.clearServer()
        Session.shared.token = nil
        // Artwork is cached per origin, but the bytes belong to the server that
        // served them; leaving them behind wastes memory the next server will
        // want for its own.
        AuthImageCache.shared.clear()
        phase = .needServer
        discovery.restart()
    }

    private func retrySavedSession(at recovered: RecoveredServer, token: String) async {
        origin = recovered.origin
        serverName = recovered.name
        api = PlurxAPI(origin: recovered.origin)
        Session.shared.origin = recovered.origin
        Session.shared.token = token
        // The same server instance at a new address: a move, not a change of
        // identity, so this token stays with it. `matchesSavedServer` already
        // proved the instance id matches before we got here.
        settings.setServer(
            origin: recovered.origin,
            instanceId: recovered.instanceId,
            token: token
        )

        do {
            username = try await requireAPI().me().username
            phase = .ready
            discovery.stop()
            await loadHome()
        } catch APIError.http(let code) where code == 401 || code == 403 {
            Session.shared.token = nil
            settings.clearToken()
            phase = .needLogin
        } catch {
            showReconnectFailure()
        }
    }

    private func rediscoverSavedServer(
        expectedInstanceId: String?,
        savedOrigin: String
    ) async -> RecoveredServer? {
        let candidates = await discovery.availableServers()
        for candidate in candidates {
            guard let candidateOrigin = try? await discovery.resolve(candidate),
                  let info = try? await PlurxAPI(origin: candidateOrigin).serverInfo(),
                  Self.matchesSavedServer(
                    candidateInstanceId: info.instanceId,
                    expectedInstanceId: expectedInstanceId,
                    savedOrigin: savedOrigin
                  ) else { continue }
            return RecoveredServer(
                origin: candidateOrigin,
                instanceId: info.instanceId,
                name: info.name
            )
        }
        return nil
    }

    private func backfillServerIdentityIfNeeded() async {
        guard settings.instanceId == nil,
              let info = try? await requireAPI().serverInfo() else { return }
        settings.instanceId = info.instanceId
        serverName = info.name
    }

    private func showReconnectFailure() {
        authError = "Couldn't reach \(serverName ?? origin). I also searched this network for the saved server."
        phase = .reconnectFailed
    }

    nonisolated static func matchesSavedServer(
        candidateInstanceId: String?,
        expectedInstanceId: String?,
        savedOrigin: String
    ) -> Bool {
        guard let candidate = candidateInstanceId?.lowercased(), !candidate.isEmpty else {
            return false
        }
        if let expected = expectedInstanceId?.lowercased(), !expected.isEmpty {
            return candidate == expected
        }

        // Migration for builds that saved `plurx-<first 12 id chars>.local`
        // before the stable instance id was persisted separately.
        guard let host = URLComponents(string: savedOrigin)?.host?.lowercased(),
              host.hasPrefix("plurx-"), host.hasSuffix(".local") else { return false }
        let start = host.index(host.startIndex, offsetBy: "plurx-".count)
        let end = host.index(host.endIndex, offsetBy: -".local".count)
        let savedPrefix = String(host[start..<end])
        let candidatePrefix = candidate
            .filter { $0.isASCII && ($0.isLetter || $0.isNumber) }
            .prefix(12)
        return !savedPrefix.isEmpty && savedPrefix == String(candidatePrefix)
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

    /// A change takes effect on the next title. `PlayerController` reads this
    /// once, when playback starts, so flipping the setting mid-film never
    /// rebuilds the stream under the person watching it.
    func setSubtitleReadiness(_ readiness: SubtitleReadiness) {
        subtitleReadiness = readiness
        settings.subtitleReadiness = readiness
    }

    func setTheme(_ theme: ViewerTheme) {
        // Write before publishing so adaptive UIKit colors resolve the new
        // palette during the SwiftUI redraw triggered by this assignment.
        settings.theme = theme
        self.theme = theme
    }

    func setAppearance(_ appearance: ViewerAppearance) {
        settings.appearance = appearance
        self.appearance = appearance
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

    /// Page a collection, handing the caller everything received so far — in
    /// the requested order — as each page lands. A thousand-item library used
    /// to sit behind a spinner for five sequential round trips before showing
    /// anything; now the first page paints after the first one. `publish` is
    /// never called with an empty list while pages are still arriving, so a
    /// refresh over a populated grid replaces it rather than blanking it.
    func libraryItems(
        _ collection: LibraryCollection,
        sort: LibrarySort,
        publish: ([Item]) -> Void
    ) async throws {
        var merged: [Item] = []
        do {
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
                    if !batch.isEmpty {
                        publish(merged.sorted { Self.compare($0, $1, sort: sort) })
                    }
                    let total = page.total ?? batch.count
                    if batch.isEmpty || offset >= total || batch.count < limit { break }
                }
            }
        } catch {
            noteAuthFailure(error)
            throw error
        }
        // An empty collection still has to clear whatever was on screen.
        if merged.isEmpty { publish([]) }
    }

    func search(_ query: String) async throws -> [Item] {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }
        do {
            return try await requireAPI().search(trimmed).results ?? []
        } catch {
            noteAuthFailure(error)
            throw error
        }
    }

    func itemDetail(_ id: Int) async throws -> ItemDetail {
        do {
            return try await requireAPI().item(id)
        } catch {
            noteAuthFailure(error)
            throw error
        }
    }

    /// Resolve a show or season to the episode its primary TV action should
    /// play. Single-season servers may expose episodes directly under a show,
    /// so that shape remains playable instead of becoming a dead detail page.
    func seriesPlayback(_ detail: ItemDetail) async -> PlayContext? {
        switch detail.item.kind {
        case "season":
            return await playableEpisode(from: Self.orderedEpisodeCandidates(detail.children ?? []))
        case "show":
            let children = detail.children ?? []
            let seasons = Self.orderedSeasonCandidates(children)
            if seasons.isEmpty {
                return await playableEpisode(from: Self.orderedEpisodeCandidates(children))
            }
            for season in seasons {
                guard let seasonDetail = try? await itemDetail(season.id) else { continue }
                if let target = await playableEpisode(
                    from: Self.orderedEpisodeCandidates(seasonDetail.children ?? [])
                ) {
                    return target
                }
            }
            return nil
        default:
            return nil
        }
    }

    static func orderedEpisodeCandidates(_ items: [Item]) -> [Item] {
        let episodes = items.filter { $0.kind == "episode" }
        let inProgress = episodes.filter {
            $0.watch?.watched != true && ($0.watch?.positionMs ?? 0) > 3_000
        }
        let unwatched = episodes.filter {
            $0.watch?.watched != true && ($0.watch?.positionMs ?? 0) <= 3_000
        }
        let watched = episodes.filter { $0.watch?.watched == true }
        return inProgress + unwatched + watched
    }

    static func orderedSeasonCandidates(_ items: [Item]) -> [Item] {
        let seasons = items.filter { $0.kind == "season" }
        let inProgress = seasons.filter {
            guard let rollup = $0.rollup, rollup.leaves > 0 else { return false }
            return rollup.watched > 0 && rollup.watched < rollup.leaves
        }
        let notStarted = seasons.filter { ($0.rollup?.watched ?? 0) == 0 }
        let completed = seasons.filter { !inProgress.contains($0) && !notStarted.contains($0) }
        return inProgress + notStarted + completed
    }

    static func resumableStartMs(positionMs: Int, durationMs: Int?) -> Int {
        guard positionMs > 3_000 else { return 0 }
        if let durationMs, durationMs > 0, Double(positionMs) > Double(durationMs) * 0.95 {
            return 0
        }
        return positionMs
    }

    private func playableEpisode(from episodes: [Item]) async -> PlayContext? {
        for episode in episodes {
            guard let detail = try? await itemDetail(episode.id),
                  let file = detail.files?.first else { continue }
            let playable = detail.item
            let durationMs = file.durationMs ?? playable.runtimeMs ?? episode.runtimeMs ?? 0
            let positionMs = playable.watch?.positionMs ?? episode.watch?.positionMs ?? 0
            return PlayContext(
                itemId: playable.id,
                fileId: file.id,
                startMs: Self.resumableStartMs(positionMs: positionMs, durationMs: durationMs),
                durationMs: durationMs,
                title: playable.title,
                subtitle: nextEpisodeSubtitle(playable),
                year: playable.year,
                overview: playable.overview
            )
        }
        return nil
    }

    func setWatched(itemId: Int, watched: Bool) async throws {
        do {
            _ = try await requireAPI().setWatched(itemId: itemId, watched: watched)
        } catch {
            noteAuthFailure(error)
            throw error
        }
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
        let playable = detail.item
        return PlayContext(
            itemId: playable.id,
            fileId: file.id,
            startMs: 0,
            durationMs: file.durationMs ?? playable.runtimeMs ?? 0,
            title: playable.title,
            subtitle: nextEpisodeSubtitle(playable),
            year: playable.year,
            overview: playable.overview
        )
    }

    private func nextEpisodeSubtitle(_ item: Item) -> String? {
        var parts: [String] = []
        if let show = item.showTitle, !show.isEmpty { parts.append(show) }
        if let season = item.seasonNumber, let episode = item.episodeNumber {
            parts.append("S\(season) E\(episode)")
        }
        return parts.isEmpty ? nil : parts.joined(separator: "  ·  ")
    }

    func decision(fileId: Int) async throws -> Decision {
        do {
            return try await requireAPI().decision(fileId: fileId, caps: caps())
        } catch {
            noteAuthFailure(error)
            throw error
        }
    }

    func createHlsSession(fileId: Int, body: CreateSessionRequest) async throws -> HlsStart {
        do {
            return try await requireAPI().createHlsSession(fileId: fileId, body: body)
        } catch {
            noteAuthFailure(error)
            throw error
        }
    }

    func hlsStatus(_ sessionId: String) async throws -> PlaybackSessionStatus {
        do {
            return try await requireAPI().hlsStatus(sessionId: sessionId)
        } catch {
            noteAuthFailure(error)
            throw error
        }
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

    /// A show has no watch row of its own — the state lives on its episodes,
    /// which are not in a library page — so filtering a TV grid on `watch`
    /// alone left "Watched" and "In progress" empty and listed finished series
    /// under "Unwatched". `list_items` now attaches the same `rollup`
    /// (`leaves` / `watched`) the detail endpoint has always returned for
    /// containers, and a container answers from it, exactly as
    /// `orderedSeasonCandidates` already reasons about seasons. Leaves keep a
    /// rollup-free `watch` row and are unaffected.
    nonisolated static func watchState(of item: Item) -> WatchState {
        if let rollup = item.rollup, rollup.leaves > 0 {
            if rollup.watched >= rollup.leaves { return .watched }
            return rollup.watched > 0 ? .inProgress : .unwatched
        }
        if item.watch?.watched == true { return .watched }
        return (item.watch?.positionMs ?? 0) > 3_000 ? .inProgress : .unwatched
    }

    nonisolated static func matches(_ item: Item, filter: WatchFilter) -> Bool {
        switch filter {
        case .all: return true
        case .unwatched: return watchState(of: item) == .unwatched
        case .inProgress: return watchState(of: item) == .inProgress
        case .watched: return watchState(of: item) == .watched
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

private struct RecoveredServer {
    let origin: String
    let instanceId: String?
    let name: String?
}
