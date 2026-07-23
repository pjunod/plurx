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
    @Published var libraries: [Library] = []
    @Published var homeLoading = true
    @Published var homeError: String?

    @Published var audioLang: String
    @Published var subLang: String

    private(set) var origin: String
    private(set) var username: String?
    private(set) var serverName: String?

    private let settings = SettingsStore()
    private var api: PlurxAPI?

    init() {
        origin = settings.origin
        username = settings.username
        audioLang = settings.audioLang
        subLang = settings.subLang
        Task { await bootstrap() }
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
            await loadHome()
        } catch {
            Session.shared.token = nil       // token rotated / server reset
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
            await loadHome()
        } catch {
            authError = "Wrong username or password"
        }
    }

    func loadHome() async {
        homeLoading = true
        homeError = nil
        do {
            async let h = requireAPI().hubs()
            async let l = requireAPI().libraries()
            hubs = try await h
            libraries = try await l
            homeLoading = false
        } catch {
            homeError = (error as? LocalizedError)?.errorDescription ?? "Failed to load"
            homeLoading = false
        }
    }

    func logout() {
        settings.clearToken()
        Session.shared.token = nil
        hubs = Hubs()
        libraries = []
        phase = .needLogin
    }

    func changeServer() {
        Session.shared.token = nil
        phase = .needServer
    }

    func setLanguages(audio: String, sub: String) {
        audioLang = audio
        subLang = sub
        settings.audioLang = audio
        settings.subLang = sub
    }

    // MARK: - Screen loaders

    func libraryItems(_ id: Int) async throws -> [Item] {
        try await requireAPI().libraryItems(id).items ?? []
    }

    func itemDetail(_ id: Int) async throws -> ItemDetail {
        try await requireAPI().item(id)
    }

    func decision(fileId: Int) async throws -> Decision {
        try await requireAPI().decision(fileId: fileId, caps: caps())
    }

    func hlsStart(fileId: Int, height: Int, start: Double, audio: Int?) async throws -> HlsStart {
        try await requireAPI().hlsStart(fileId: fileId, height: height, start: start, audio: audio)
    }

    /// Best-effort — a dropped progress beat shouldn't surface an error.
    func reportProgress(itemId: Int, positionMs: Int, durationMs: Int?) async {
        try? await requireAPI().progress(itemId: itemId, positionMs: positionMs, durationMs: durationMs)
    }

    private static func normalizeOrigin(_ raw: String) -> String {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return s }
        if !s.hasPrefix("http://") && !s.hasPrefix("https://") { s = "http://" + s }
        while s.hasSuffix("/") { s.removeLast() }
        return s
    }
}
