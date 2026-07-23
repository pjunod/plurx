import Foundation

/// Live connection state. The origin + token are set once at connect/login and
/// read wherever a request, image, or media URL is built. Two auth shapes:
/// API/image requests carry `Authorization: Bearer`, while AVPlayer URLs (which
/// can't set headers) carry the token inline as `?token=` — both accepted by
/// the server's `AuthUser` extractor.
final class Session: @unchecked Sendable {
    static let shared = Session()

    /// Server origin, no trailing slash, e.g. `http://192.168.1.10:32600`.
    var origin: String = ""
    /// Bearer token, or nil when signed out.
    var token: String?

    /// Absolute URL for a server-relative path.
    func url(_ path: String) -> URL? {
        if path.hasPrefix("http") { return URL(string: path) }
        return URL(string: origin + path)
    }

    /// Absolute URL with the token inline — for AVPlayer / `<img>`-style loads
    /// that can't set an Authorization header. Capability-authed HLS playlists
    /// (which already carry an unguessable session id) don't need this.
    func mediaURL(_ path: String) -> URL? {
        guard let base = url(path) else { return nil }
        guard let token, !path.hasPrefix("http") else { return base }
        var comps = URLComponents(url: base, resolvingAgainstBaseURL: false)
        var items = comps?.queryItems ?? []
        items.append(URLQueryItem(name: "token", value: token))
        comps?.queryItems = items
        return comps?.url ?? base
    }

    /// Add the bearer header to an API/image request.
    func authorize(_ request: inout URLRequest) {
        if let token {
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
    }
}
