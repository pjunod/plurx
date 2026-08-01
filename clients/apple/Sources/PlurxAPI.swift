import Foundation

enum APIError: Error, LocalizedError {
    case badURL
    case http(Int)
    case transport(String)

    var errorDescription: String? {
        switch self {
        case .badURL: return "Invalid server address"
        case .http(let code): return "Server returned \(code)"
        case .transport(let message): return message
        }
    }
}

/// Async `/api/v1` client over URLSession. The bearer token is added per-request
/// from `Session`; JSON uses snake_case ⇄ camelCase conversion so the Swift
/// models stay idiomatic.
struct PlurxAPI {
    let origin: String
    /// A local-network request may be the operation that causes iOS to show
    /// its permission sheet. The first request must wait for that choice,
    /// rather than failing underneath the sheet and making login work only on
    /// the second attempt.
    private static let waitingSession: URLSession = {
        let configuration = URLSessionConfiguration.default
        configuration.waitsForConnectivity = true
        configuration.timeoutIntervalForRequest = 30
        configuration.timeoutIntervalForResource = 30
        return URLSession(configuration: configuration)
    }()
    private var session: URLSession { Self.waitingSession }

    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()
    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()

    private func makeURL(_ path: String, query: [URLQueryItem] = []) -> URL? {
        guard var comps = URLComponents(string: origin + "/api/v1/" + path) else { return nil }
        if !query.isEmpty { comps.queryItems = query }
        return comps.url
    }

    private func get<T: Decodable>(_ path: String, query: [URLQueryItem] = []) async throws -> T {
        guard let url = makeURL(path, query: query) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        Session.shared.authorize(&req)
        return try await run(req)
    }

    private func post<B: Encodable, T: Decodable>(_ path: String, body: B) async throws -> T {
        var req = try jsonRequest(path, body: body)
        Session.shared.authorize(&req)
        return try await run(req)
    }

    private func post<T: Decodable>(_ path: String) async throws -> T {
        guard let url = makeURL(path) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        Session.shared.authorize(&req)
        return try await run(req)
    }

    private func postNoContent<B: Encodable>(_ path: String, body: B) async throws {
        var req = try jsonRequest(path, body: body)
        Session.shared.authorize(&req)
        let (_, resp) = try await session.data(for: req)
        try Self.check(resp)
    }

    private func jsonRequest<B: Encodable>(_ path: String, body: B) throws -> URLRequest {
        guard let url = makeURL(path) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try Self.encoder.encode(body)
        return req
    }

    private func run<T: Decodable>(_ req: URLRequest) async throws -> T {
        let data: Data
        let resp: URLResponse
        do { (data, resp) = try await session.data(for: req) }
        catch { throw Self.transportError(from: error) }
        try Self.check(resp)
        return try Self.decoder.decode(T.self, from: data)
    }

    /// Cancellation is control flow, not a server failure. Keeping it typed
    /// lets view models distinguish SwiftUI replacing a task from the server
    /// actually becoming unreachable. URLSession reports cancellation as
    /// either Swift's CancellationError or NSURLErrorCancelled depending on
    /// which layer observes it first.
    static func transportError(from error: Error) -> Error {
        if error is CancellationError { return CancellationError() }
        if let urlError = error as? URLError, urlError.code == .cancelled {
            return CancellationError()
        }
        return APIError.transport(error.localizedDescription)
    }

    private static func check(_ resp: URLResponse) throws {
        if let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw APIError.http(http.statusCode)
        }
    }

    // MARK: - Endpoints

    /// Server identity is public and is also used to verify a rediscovered
    /// endpoint. Never attach a saved bearer token while probing candidates.
    func serverInfo() async throws -> ServerInfo {
        guard let url = makeURL("server") else { throw APIError.badURL }
        return try await run(URLRequest(url: url))
    }
    func login(_ body: LoginRequest) async throws -> LoginResponse { try await post("auth/login", body: body) }
    func me() async throws -> User { try await get("me") }
    func libraries() async throws -> [Library] { try await get("libraries") }

    func libraryItems(
        _ id: Int,
        sort: LibrarySort = .title,
        offset: Int = 0,
        limit: Int = 200
    ) async throws -> Page {
        try await get("libraries/\(id)/items", query: [
            URLQueryItem(name: "limit", value: String(limit)),
            URLQueryItem(name: "offset", value: String(offset)),
            URLQueryItem(name: "sort", value: sort.rawValue),
        ])
    }

    func hubs() async throws -> Hubs { try await get("hubs") }
    func comingSoon() async throws -> ComingSoonResponse { try await get("coming-soon") }
    func item(_ id: Int) async throws -> ItemDetail { try await get("items/\(id)") }

    func setWatched(itemId: Int, watched: Bool) async throws -> MutationResponse {
        try await post("items/\(itemId)/\(watched ? "scrobble" : "unscrobble")")
    }

    func search(_ query: String, limit: Int = 200) async throws -> SearchResponse {
        try await get("search", query: [
            URLQueryItem(name: "q", value: query),
            URLQueryItem(name: "limit", value: String(limit)),
        ])
    }

    func decision(fileId: Int, caps: [URLQueryItem]) async throws -> Decision {
        try await get("files/\(fileId)/decision", query: caps)
    }

    /// POST rather than the deprecated GET bridge: creating a session spawns a
    /// process and kills its predecessor, and anything entitled to replay a
    /// GET could spawn a second encoder. The body carries this player's
    /// `playback_id` and a per-attempt `request_id` so a replay recovers the
    /// same session instead.
    func createHlsSession(fileId: Int, body: CreateSessionRequest) async throws -> HlsStart {
        try await post("files/\(fileId)/hls/sessions", body: body)
    }

    func hlsStatus(sessionId: String) async throws -> PlaybackSessionStatus {
        try await get("hls/\(sessionId)/status")
    }

    /// DELETE the session the moment playback ends. Without it the encoder
    /// lives on for the idle timeout plus a reaper tick — a hardware slot
    /// held for over a minute for nobody. Best-effort by design: the route is
    /// idempotent and capability-authed, and a failure to say goodbye is not
    /// worth surfacing to a viewer who has already left.
    func endHlsSession(_ sessionId: String) async {
        guard let url = makeURL("hls/\(sessionId)") else { return }
        var req = URLRequest(url: url)
        req.httpMethod = "DELETE"
        Session.shared.authorize(&req)
        _ = try? await session.data(for: req)
    }

    func progress(itemId: Int, positionMs: Int, durationMs: Int?) async throws {
        try await postNoContent(
            "items/\(itemId)/progress",
            body: ProgressRequest(positionMs: positionMs, durationMs: durationMs)
        )
    }
}
