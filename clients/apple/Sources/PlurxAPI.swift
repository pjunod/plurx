import Foundation

enum APIError: Error, LocalizedError {
    case badURL
    case http(Int)
    /// A transport failure already placed in the shared taxonomy
    /// (docs/CLIENT-CONNECTIVITY.md §1). Every user-facing transport error is
    /// this case; the raw `URLError` never travels further than
    /// `transportError(from:)`.
    case connection(ConnectionFailure)
    /// Cinema's own sentence for a condition outside the taxonomy — "Not
    /// enough device storage for this download" and friends. It has never
    /// carried a Foundation string since the classifier landed, and must not
    /// start again.
    case transport(String)

    var errorDescription: String? {
        switch self {
        case .badURL: return "Invalid server address"
        case .http(let code): return "Server returned \(code)"
        // No server is in scope here, so `{server}` resolves to the contract's
        // fallback. Screens that know which server they were talking to call
        // `Connectivity.copy(for:server:)` directly and get its name.
        case .connection(let failure): return Connectivity.copy(for: failure, server: nil).short
        case .transport(let message): return message
        }
    }
}

/// Async `/api/v1` client over URLSession. The bearer token is added per-request
/// from `Session`; JSON uses snake_case ⇄ camelCase conversion so the Swift
/// models stay idiomatic.
struct PlurxAPI {
    let origin: String
    /// A cold embedded-subtitle extraction can require one full sequential
    /// read of a large MKV before the HLS session exists. Keep ordinary API
    /// calls brisk, but let an explicit playback-preparation action finish.
    static let playbackPreparationTimeout: TimeInterval = 180
    /// docs/CLIENT-CONNECTIVITY.md §3. Named rather than written inline so the
    /// deadlines are assertable: an error state nobody ever reaches is worse
    /// than a bad error message, and a deadline quietly widened again is how
    /// that comes back.
    static let apiRequestTimeout: TimeInterval = 15
    static let apiResourceTimeout: TimeInterval = 30
    /// docs/CLIENT-CONNECTIVITY.md §2.3 — read that section before changing
    /// this. `waitsForConnectivity` is what covers the local-network
    /// permission sheet: on a fresh install the first request is made while
    /// iOS is still asking, and a session that does not wait fails underneath
    /// the sheet, so connecting works only on the second tap. The Bonjour
    /// preflight in `AppModel.bootstrap()` predates this flag and was judged
    /// insufficient — the flag is the fix, not redundant with it.
    ///
    /// The price, named in §2.3: `.notConnectedToInternet` never surfaces, so
    /// a fully offline device classifies as `timeout` rather than `offline`.
    /// `apiResourceTimeout` is what keeps that honest instead of endless —
    /// *No answer from the server* after 30 seconds, not a spinner forever.
    static let apiWaitsForConnectivity = true
    private static let apiSession: URLSession = {
        let configuration = URLSessionConfiguration.default
        configuration.waitsForConnectivity = apiWaitsForConnectivity
        configuration.timeoutIntervalForRequest = apiRequestTimeout
        configuration.timeoutIntervalForResource = apiResourceTimeout
        return URLSession(configuration: configuration)
    }()
    private static let playbackPreparationSession: URLSession = {
        let configuration = URLSessionConfiguration.default
        configuration.waitsForConnectivity = true
        configuration.timeoutIntervalForRequest = playbackPreparationTimeout
        configuration.timeoutIntervalForResource = playbackPreparationTimeout
        return URLSession(configuration: configuration)
    }()
    private var session: URLSession { Self.apiSession }

    /// docs/CLIENT-CONNECTIVITY.md §3. Two endpoints look like ordinary JSON
    /// calls and are not, so they take the playback deadline rather than the
    /// API one:
    ///
    /// - `/decision` reaches `markers_for` server-side, which can fall through
    ///   to a live `ffprobe -show_chapters` subprocess with no timeout of its
    ///   own, behind an availability stat that may be sitting on a spun-down
    ///   NAS. A 15-second deadline there turns a slow first play of a large
    ///   file into *No answer from the server* — an error state for something
    ///   that was merely working. The web client gives it 120 s for the same
    ///   reason.
    /// - Session open spawns an encoder process and kills its predecessor.
    ///
    /// Applied by `get`/`post`/`put` from the path itself, not passed in by the
    /// endpoint methods: an opt-in `using:` argument is a deadline one deleted
    /// line reverts, and the suite would stay green because a predicate is not
    /// its own wiring.
    nonisolated static func usesPlaybackDeadline(path: String) -> Bool {
        path.hasSuffix("/decision") || path.hasSuffix("/hls/sessions")
    }

    private static func deadlineSession(forPath path: String) -> URLSession {
        usesPlaybackDeadline(path: path) ? playbackPreparationSession : apiSession
    }

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

    // Every path-taking request builder below resolves its own deadline from
    // `deadlineSession(forPath:)`. There is deliberately no `using:` parameter
    // for an endpoint method to forget: when the long deadline was opt-in per
    // call site, deleting one argument put `/decision` back on 15 seconds with
    // every test still green, because a predicate is not its own wiring.

    private func get<T: Decodable>(
        _ path: String,
        query: [URLQueryItem] = []
    ) async throws -> T {
        guard let url = makeURL(path, query: query) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        Session.shared.authorize(&req)
        return try await run(req, using: Self.deadlineSession(forPath: path))
    }

    private func post<B: Encodable, T: Decodable>(
        _ path: String,
        body: B
    ) async throws -> T {
        var req = try jsonRequest(path, body: body)
        Session.shared.authorize(&req)
        return try await run(req, using: Self.deadlineSession(forPath: path))
    }

    private func post<T: Decodable>(_ path: String) async throws -> T {
        guard let url = makeURL(path) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        Session.shared.authorize(&req)
        return try await run(req, using: Self.deadlineSession(forPath: path))
    }

    private func put<B: Encodable, T: Decodable>(_ path: String, body: B) async throws -> T {
        var req = try jsonRequest(path, body: body)
        req.httpMethod = "PUT"
        Session.shared.authorize(&req)
        return try await run(req, using: Self.deadlineSession(forPath: path))
    }

    private func deleteNoContent(_ path: String) async throws {
        guard let url = makeURL(path) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        req.httpMethod = "DELETE"
        Session.shared.authorize(&req)
        let (_, resp) = try await send(req, using: Self.deadlineSession(forPath: path))
        try Self.check(resp)
    }

    private func postNoContent(_ path: String) async throws {
        guard let url = makeURL(path) else { throw APIError.badURL }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        Session.shared.authorize(&req)
        let (_, resp) = try await send(req, using: Self.deadlineSession(forPath: path))
        try Self.check(resp)
    }

    private func postNoContent<B: Encodable>(_ path: String, body: B) async throws {
        var req = try jsonRequest(path, body: body)
        Session.shared.authorize(&req)
        let (_, resp) = try await send(req, using: Self.deadlineSession(forPath: path))
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

    /// Every request whose failure can reach a screen leaves this type here.
    /// Calling `session.data(for:)` directly is what let a raw `URLError` — and
    /// with it a Foundation sentence — escape from the no-content routes
    /// untyped. The one remaining direct call is `endHlsSession`, which is
    /// best-effort and discards its result and its error entirely.
    private func send(
        _ req: URLRequest,
        using session: URLSession? = nil
    ) async throws -> (Data, URLResponse) {
        do { return try await (session ?? self.session).data(for: req) }
        catch { throw Self.transportError(from: error) }
    }

    private func run<T: Decodable>(
        _ req: URLRequest,
        using session: URLSession? = nil
    ) async throws -> T {
        let (data, resp) = try await send(req, using: session)
        try Self.check(resp)
        return try Self.decoder.decode(T.self, from: data)
    }

    /// Cancellation is control flow, not a server failure. Keeping it typed
    /// lets view models distinguish SwiftUI replacing a task from the server
    /// actually becoming unreachable. URLSession reports cancellation as
    /// either Swift's CancellationError or NSURLErrorCancelled depending on
    /// which layer observes it first.
    ///
    /// Everything else is placed in the shared taxonomy here, once, so no
    /// screen has to look at a `URLError` and none can render Foundation's
    /// wording (docs/CLIENT-CONNECTIVITY.md §2.3).
    static func transportError(from error: Error) -> Error {
        if error is CancellationError { return CancellationError() }
        if let urlError = error as? URLError, urlError.code == .cancelled {
            return CancellationError()
        }
        // `classify` only declines cancellation — handled above — and HTTP
        // 401/403, which a transport failure is not, so this is total in
        // practice; `.unknown` is the contract's floor rather than a guess.
        return APIError.connection(Connectivity.classify(error) ?? .unknown)
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
        // The 180 s deadline comes from `deadlineSession(forPath:)` inside
        // `get`, not from anything this call site remembers to pass.
        try await get("files/\(fileId)/decision", query: caps)
    }

    func pgsOverlayManifest(
        fileId: Int,
        trackIndex: Int
    ) async throws -> PGSOverlayManifestFetch {
        guard let url = makeURL("files/\(fileId)/subs/\(trackIndex)/overlay.json") else {
            throw APIError.badURL
        }
        var request = URLRequest(url: url)
        Session.shared.authorize(&request)
        let (data, response) = try await send(request)
        guard let http = response as? HTTPURLResponse else {
            throw APIError.transport("The PGS overlay response was not HTTP.")
        }
        switch PGSOverlayPolicy.manifestDisposition(http.statusCode) {
        case .ready:
            return .ready(try Self.decoder.decode(PGSOverlayManifest.self, from: data))
        case .preparing where http.statusCode == 202:
            let state = try Self.decoder.decode(PGSOverlayPreparing.self, from: data)
            guard state.state == "preparing" else { throw PGSOverlayError.invalidManifest }
            return .preparing(retryAfterMs: min(max(250, state.retryAfterMs), 5_000))
        case .preparing:
            return .preparing(retryAfterMs: PGSOverlayPolicy.retryAfterMs(
                http.value(forHTTPHeaderField: "Retry-After")
            ))
        case .terminal:
            throw APIError.http(http.statusCode)
        }
    }

    func pgsOverlayObject(
        fileId: Int,
        trackIndex: Int,
        generation: String,
        path: String
    ) async throws -> Data {
        guard PGSOverlayManifest.objectHash(from: path, generation: generation) != nil,
              let url = makeURL("files/\(fileId)/subs/\(trackIndex)/\(path)")
        else { throw PGSOverlayError.invalidManifest }
        var request = URLRequest(url: url)
        Session.shared.authorize(&request)
        let (data, response) = try await send(request)
        try Self.check(response)
        guard let http = response as? HTTPURLResponse,
              http.value(forHTTPHeaderField: "Content-Type")?
                .lowercased().hasPrefix("image/png") == true
        else { throw PGSOverlayError.invalidImage }
        return data
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

    func offlineOptions(
        fileId: Int,
        audioLanguage: String,
        subtitleLanguage: String,
        subtitleMode: String = "auto"
    ) async throws -> OfflineOptions {
        try await get("files/\(fileId)/offline-options", query: [
            URLQueryItem(name: "audio_lang", value: audioLanguage),
            URLQueryItem(name: "subtitle_lang", value: subtitleLanguage),
            URLQueryItem(name: "subtitle_mode", value: subtitleMode),
        ])
    }

    func createOfflinePackage(
        fileId: Int,
        body: CreateOfflinePackageRequest
    ) async throws -> OfflinePackageStatus {
        try await post("files/\(fileId)/offline-packages", body: body)
    }

    func offlinePackage(_ packageId: String) async throws -> OfflinePackageStatus {
        try await get("offline/packages/\(packageId)")
    }

    func putOfflineLease(
        packageId: String,
        token: String
    ) async throws -> OfflineLeaseResponse {
        try await put(
            "offline/packages/\(packageId)/lease",
            body: OfflineLeaseRequest(token: token)
        )
    }

    func deleteOfflinePackage(_ packageId: String) async throws {
        try await deleteNoContent("offline/packages/\(packageId)")
    }

    func completeOfflinePackage(_ packageId: String) async throws {
        try await postNoContent("offline/packages/\(packageId)/complete")
    }

    func absoluteOfflineManifest(_ path: String) -> URL? {
        guard let base = URL(string: origin) else { return nil }
        return URL(string: path, relativeTo: base)?.absoluteURL
    }

    func progress(
        itemId: Int,
        positionMs: Int,
        durationMs: Int?,
        recordedAt: Int? = nil
    ) async throws {
        try await postNoContent(
            "items/\(itemId)/progress",
            body: ProgressRequest(
                positionMs: positionMs,
                durationMs: durationMs,
                recordedAt: recordedAt
            )
        )
    }
}
