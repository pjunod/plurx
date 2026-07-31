import Foundation

/// Wire models — a subset of plurx's native `/api/v1` JSON (see crates/plurxd
/// http/dto.rs). Decoded with `.convertFromSnakeCase`, so `position_ms` →
/// `positionMs`, etc. Optional scalars decode to nil when absent; array fields
/// are optional and coalesced with `?? []` at use sites, so a server that omits
/// or adds a field still decodes.

struct ServerInfo: Codable {
    var setupRequired: Bool?
    var name: String?
    var version: String?
}

struct User: Codable {
    let id: Int
    let username: String
    var isAdmin: Bool?
}

struct LoginResponse: Codable {
    let token: String
    let user: User
}

struct Library: Codable, Identifiable, Hashable {
    let id: Int
    let name: String
    let kind: String
    var anime: Bool?
}

struct Watch: Codable, Hashable {
    var positionMs: Int?
    var durationMs: Int?
    var watched: Bool?
}

struct Item: Codable, Identifiable, Hashable {
    let id: Int
    let kind: String
    let title: String
    var year: Int?
    var overview: String?
    var poster: String?
    var backdrop: String?
    var seasonNumber: Int?
    var episodeNumber: Int?
    var showTitle: String?
    var runtimeMs: Int?
    var watch: Watch?

    var isMovieOrEpisode: Bool { kind == "movie" || kind == "episode" }
}

struct Hubs: Codable {
    var continueWatching: [Item]?
    var nextUp: [Item]?
    var recentlyAdded: [Item]?
}

struct Page: Codable {
    var items: [Item]?
    var total: Int?
}

struct MediaFile: Codable, Identifiable {
    let id: Int
    var filename: String?
    var durationMs: Int?
    var container: String?
    var videoCodec: String?
    var width: Int?
    var height: Int?
}

struct ItemDetail: Codable {
    let item: Item
    var files: [MediaFile]?
    var children: [Item]?
    var ancestors: [Item]?
}

struct Marker: Codable, Hashable {
    let kind: String
    let label: String
    let startMs: Int
    let endMs: Int
    var chapter: Bool?
}

/// The server-owned execution plan for a verdict (`DecisionResponse.delivery`
/// in crates/plurxd http/stream.rs). The client *executes* this rather than
/// re-deriving policy from `method` — re-deriving is how this app came to
/// re-encode remux verdicts at a hardcoded 1080p.
struct Delivery: Codable {
    let mode: String            // "direct" | "remux" | "transcode"
    var url: String?            // direct: the file; remux: progressive fMP4
    var sessionsUrl: String?    // POST target for a copy or transcode session
    var aac: Bool?              // remux: the copy session must re-encode audio
}

struct Decision: Codable {
    let fileId: Int
    let method: String          // "direct_play" | "remux" | "transcode"
    let playUrl: String
    var delivery: Delivery?
    var reasons: [String]?
    var transcodeAudio: Bool?
    var markers: [Marker]?
    var audioOffsetMs: Int?
}

struct HlsStart: Codable {
    let sessionId: String
    let playlistUrl: String
    var durationMs: Int?
    var startSeconds: Double?
    var encoder: String?
}

// MARK: - Request bodies

struct LoginRequest: Codable {
    let username: String
    let password: String
    var device: String = "Apple"
}

/// Body for `POST /files/{id}/hls/sessions`. Optionals encode as absent
/// (synthesized Codable uses `encodeIfPresent`), which matters for `height`:
/// omitting it selects the server's Auto rung — the rung depends on which
/// encoder wins, and only the create response knows that — so this client
/// never names a height at all.
struct CreateSessionRequest: Codable {
    /// Stable for one player instance; supersession is keyed by it.
    let playbackId: String
    /// Fresh per attempt: makes a replayed create return the same session
    /// instead of spawning a second encoder.
    var requestId: String? = UUID().uuidString
    var height: Int?
    var start: Double?
    var audio: Int?
    /// Copy the source video into HLS untouched (the remux plan).
    var copy: Bool?
    /// With `copy`: re-encode the audio the client can't take.
    var aac: Bool?
}

struct ProgressRequest: Codable {
    let positionMs: Int
    var durationMs: Int?
}
