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
    private let session: URLSession = .shared

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
        catch { throw APIError.transport(error.localizedDescription) }
        try Self.check(resp)
        return try Self.decoder.decode(T.self, from: data)
    }

    private static func check(_ resp: URLResponse) throws {
        if let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw APIError.http(http.statusCode)
        }
    }

    // MARK: - Endpoints

    func serverInfo() async throws -> ServerInfo { try await get("server") }
    func login(_ body: LoginRequest) async throws -> LoginResponse { try await post("auth/login", body: body) }
    func me() async throws -> User { try await get("me") }
    func libraries() async throws -> [Library] { try await get("libraries") }

    func libraryItems(_ id: Int) async throws -> Page {
        try await get("libraries/\(id)/items", query: [
            URLQueryItem(name: "limit", value: "200"),
            URLQueryItem(name: "sort", value: "title"),
        ])
    }

    func hubs() async throws -> Hubs { try await get("hubs") }
    func item(_ id: Int) async throws -> ItemDetail { try await get("items/\(id)") }

    func decision(fileId: Int, caps: [URLQueryItem]) async throws -> Decision {
        try await get("files/\(fileId)/decision", query: caps)
    }

    func hlsStart(fileId: Int, height: Int, start: Double, audio: Int?) async throws -> HlsStart {
        var q: [URLQueryItem] = [
            URLQueryItem(name: "height", value: "\(height)"),
            URLQueryItem(name: "start", value: "\(start)"),
        ]
        if let audio { q.append(URLQueryItem(name: "audio", value: "\(audio)")) }
        return try await get("files/\(fileId)/hls/start", query: q)
    }

    func progress(itemId: Int, positionMs: Int, durationMs: Int?) async throws {
        try await postNoContent(
            "items/\(itemId)/progress",
            body: ProgressRequest(positionMs: positionMs, durationMs: durationMs)
        )
    }
}
