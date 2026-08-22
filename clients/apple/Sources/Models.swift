import CoreGraphics
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
    var build: String?
    var instanceId: String?
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

struct WatchRollup: Codable, Hashable {
    var leaves: Int
    var watched: Int
}

struct Item: Codable, Identifiable, Hashable {
    let id: Int
    var libraryId: Int?
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
    var airDate: String?
    var recordedAt: String?
    var tags: [String]?
    var author: String? = nil
    var bookWorkId: String? = nil
    var bookEditionId: String? = nil
    var bookMetadataSource: String? = nil
    var resolution: Int?
    var media: ItemMedia?
    var childCount: Int?
    var addedAt: Int?
    var updatedAt: Int?
    var watch: Watch?
    var rollup: WatchRollup?

    var isMovieOrEpisode: Bool { kind == "movie" || kind == "episode" }
    var isPlayable: Bool { isMovieOrEpisode || kind == "video" || kind == "audiobook" }
    var isBook: Bool { kind == "book" }
    var isAudiobook: Bool { kind == "audiobook" }
}

/// A list-safe summary of the best file behind an item. Season detail uses
/// this block for episode cards so the client does not fan out into one detail
/// request per episode just to learn resolution and dynamic range.
struct ItemMedia: Codable, Hashable {
    var files: Int?
    var bytes: Int?
    var video: String?
    var height: Int?
    var hdr: String?
    var hdrFormat: String?
    var audio: String?
    var container: String?
}

struct Hubs: Codable {
    var continueWatching: [Item]?
    var nextUp: [Item]?
    var recentlyAdded: [Item]?
}

struct Page: Codable {
    var items: [Item]?
    var total: Int?
    var offset: Int?
    var limit: Int?
}

struct SearchResponse: Codable {
    var results: [Item]?
}

struct ComingSoonEntry: Codable, Identifiable, Hashable {
    var id: String { "\(date):\(kind):\(title):\(detail)" }
    let date: String
    let kind: String
    let title: String
    let detail: String
    let hasFile: Bool
    var poster: String?
    var itemId: Int?
}

struct ComingSoonResponse: Codable {
    var configured: Bool
    var entries: [ComingSoonEntry]
}

struct MutationResponse: Codable {
    var ok: Bool
    var updated: Int?
}

enum LibrarySort: String, CaseIterable, Identifiable {
    case title
    case added
    case year
    case resolution
    case recorded

    var id: String { rawValue }
    var label: String {
        switch self {
        case .title: return "Title (A–Z)"
        case .added: return "Recently added"
        case .year: return "Year"
        case .resolution: return "Resolution"
        case .recorded: return "Date recorded"
        }
    }
    var icon: String {
        switch self {
        case .title: return "textformat.abc"
        case .added: return "clock"
        case .year: return "calendar"
        case .resolution: return "4k.tv"
        case .recorded: return "camera"
        }
    }
}

enum WatchFilter: String, CaseIterable, Identifiable {
    case all
    case unwatched
    case inProgress
    case watched

    var id: String { rawValue }
    var label: String {
        switch self {
        case .all: return "Everything"
        case .unwatched: return "Unwatched"
        case .inProgress: return "In progress"
        case .watched: return "Watched"
        }
    }
}

/// Where one row of a library grid sits in the watch cycle. Leaves answer from
/// their own `watch` row; containers have none — the state lives on their
/// episodes, which are not in a library page at all — so they answer from the
/// `rollup` the server now attaches to them (`AppModel.watchState`).
enum WatchState: Equatable {
    case unwatched
    case inProgress
    case watched
}

enum LibraryGrouping: String, CaseIterable, Identifiable {
    case category
    case share

    var id: String { rawValue }
    var label: String { self == .category ? "Category" : "Library" }
}

/// Native themes shared with the Android viewer. Web-only catalogue themes can
/// be added here once their tokens and platform treatment have an Apple port.
enum ViewerTheme: String, CaseIterable, Identifiable {
    case classic
    case terminal
    case noirr

    var id: String { rawValue }
    var label: String {
        switch self {
        case .classic: return "Classic"
        case .terminal: return "Terminal"
        case .noirr: return "noirr"
        }
    }
}

enum ViewerAppearance: String, CaseIterable, Identifiable {
    case system
    case light
    case dark

    var id: String { rawValue }
    var label: String {
        switch self {
        case .system: return "Auto (system)"
        case .light: return "Light"
        case .dark: return "Dark"
        }
    }
}

/// When a title's text subtitles are made switchable, which is really a choice
/// about who pays for a subtitle menu nobody may open.
///
/// - `.instant` — the v0.2 behaviour. A file carrying any native text track
///   opens through a copy-HLS session, so every track already exists as a
///   rendition and turning subtitles on, changing language, or turning them off
///   is an AVPlayer media selection on the item that is already playing. The
///   cost is a server session and a segmenter on every play, even for the
///   majority that never touch the menu.
/// - `.onDemand` — the default (Paul, 2026-08-02). Direct-play the raw file,
///   and rebuild it as a copy session at the same position the first time a
///   subtitle is chosen. The cost is one restart, and only for viewers who
///   actually use subtitles.
///
/// Recorded as a deliberate tradeoff in docs/APPLE-CLIENT-PARITY.md §2.
enum SubtitleReadiness: String, CaseIterable, Identifiable {
    case instant
    case onDemand

    var id: String { rawValue }
    var label: String {
        switch self {
        case .instant: return "Instant"
        case .onDemand: return "After a short pause"
        }
    }
}

enum PosterSize: String, CaseIterable, Identifiable {
    case small = "s"
    case medium = "m"
    case large = "l"
    case extraLarge = "xl"

    var id: String { rawValue }
    var label: String {
        switch self {
        case .small: return "Small"
        case .medium: return "Medium"
        case .large: return "Large"
        case .extraLarge: return "Extra large"
        }
    }

    var posterWidth: CGFloat {
        #if os(tvOS)
        switch self {
        case .small: return 180
        case .medium: return 220
        case .large: return 260
        case .extraLarge: return 300
        }
        #else
        switch self {
        case .small: return 120
        case .medium: return 150
        case .large: return 186
        case .extraLarge: return 226
        }
        #endif
    }

    var landscapeWidth: CGFloat { posterWidth * 1.8 }
}

/// One browse destination. In category mode this merges several shares (and,
/// once Phase 4 lands, their replicated cluster storage) into one Movies or TV
/// grid while retaining each item's library provenance.
struct LibraryCollection: Identifiable, Hashable {
    let id: String
    let title: String
    let kind: String
    let libraries: [Library]

    var supportsRecordedSort: Bool { kind == "home" }
}

struct MediaFile: Codable, Identifiable {
    let id: Int
    var filename: String? = nil
    var size: Int? = nil
    var durationMs: Int? = nil
    var container: String? = nil
    var videoCodec: String? = nil
    var videoProfile: String? = nil
    var width: Int? = nil
    var height: Int? = nil
    var bitDepth: Int? = nil
    /// Coarse source grade — `"dolby_vision" | "hdr10" | "hlg"` — and the rich
    /// display label ("Dolby Vision · Profile 7 (HDR10-compatible)"). Both have
    /// always been on `FileDto`; the detail screen now shows them.
    var hdr: String? = nil
    var hdrFormat: String? = nil
    var bitrate: Int? = nil
    var audioStreams: [AudioTrack]? = nil
    var subtitleStreams: [SubtitleStream]? = nil
    /// The server's cold-start track outcome for this file. Absent from servers
    /// that predate the shared track-facts contract, in which case the detail
    /// screen shows the tracks without marking a default or a language status.
    var playbackDefaults: PlaybackDefaults? = nil
    var partOffsetMs: Int? = nil
    var chapters: [BookChapter]? = nil
    var available: Bool? = true
    /// The server's format/action registry for this exact file. Optional for
    /// compatibility with servers that predate the registry.
    var reader: ReaderCapability? = nil
    /// Exact edition identity required by a native document reader when it
    /// saves a locator. Older servers omit it and never advertise PDF Read.
    var readerRevision: ReadingRevision? = nil

    var isEpub: Bool {
        container?.lowercased() == "epub"
            || filename?.lowercased().hasSuffix(".epub") == true
    }
}

enum ReaderAction: String, Codable, Equatable {
    case read
    case openIn = "open_in"
    case unavailable
}

struct ReaderSurfaceCapability: Codable, Equatable {
    let online: ReaderAction
    let offline: ReaderAction
}

struct ReaderCapability: Codable, Equatable {
    let format: String
    let web: ReaderSurfaceCapability
    let apple: ReaderSurfaceCapability
    let android: ReaderSurfaceCapability
    let television: ReaderSurfaceCapability
}

enum BookReaderPolicy {
    static func canRead(_ file: MediaFile, onTelevision: Bool) -> Bool {
        guard !onTelevision, file.available != false else { return false }
        if let reader = file.reader {
            guard reader.apple.online == .read else { return false }
            return reader.format != "pdf" || file.readerRevision != nil
        }
        return file.isEpub
    }

    static func canDownload(_ file: MediaFile, onTelevision: Bool) -> Bool {
        guard !onTelevision, file.available != false else { return false }
        if let reader = file.reader { return reader.apple.offline == .read }
        return file.isEpub
    }
}

/// One subtitle stream exactly as item detail reports it (`FileDto`'s
/// `subtitle_streams`, i.e. plurx-core's `SubtitleStream`).
///
/// Deliberately *not* `SubtitleTrack`: that one is `/decision`'s playable track
/// and carries the HLS-rendition and overlay classifications a decision made.
/// A detail screen has no session to ask about, so it renders the container's
/// own facts and nothing more.
struct SubtitleStream: Codable, Identifiable, Hashable {
    var id: Int { index }
    let index: Int
    let codec: String
    var language: String?
    var title: String?
    var `default`: Bool
    var forced: Bool
    /// The container's `hearing_impaired` disposition — SDH. Optional because a
    /// server (or the shared contract fixture) that predates the field omits
    /// it, and because absence is not the same claim as `false`.
    var hearingImpaired: Bool?
}

/// How the configured preferred language relates to one file's tracks and to
/// the track policy actually selected — the five states of
/// `playback_defaults.*.preferred_language_status` (docs/CLIENTS.md §1).
///
/// `unknown` is not `missing`. An untagged track means the server cannot claim
/// the preferred language is absent, so neither may this client; an
/// unrecognized future state decodes here for the same reason.
enum PreferredLanguageStatus: String, Codable, Hashable {
    case selected
    case available
    case missing
    case unknown
    case noTracks = "no_tracks"

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = PreferredLanguageStatus(rawValue: raw) ?? .unknown
    }
}

struct PlaybackTrackDefault: Codable, Hashable {
    var selectedIndex: Int?
    var preferredLanguage: String?
    var preferredLanguageStatus: PreferredLanguageStatus?
}

struct PlaybackDefaults: Codable, Hashable {
    var audio: PlaybackTrackDefault?
    var subtitle: PlaybackTrackDefault?
}

/// A viewer's pre-play audio/subtitle choice, made on the detail screen and
/// spent on exactly one playback.
///
/// `nil` on a side means "no explicit choice" — the server's own policy default
/// stands, and `/decision` is asked exactly as it was before this type existed.
/// Nothing here is ever written to the server's Playback defaults: the choice
/// belongs to one playback (docs/CLIENTS.md §1).
struct PrePlaySelection: Equatable {
    var audioIndex: Int?
    /// `subtitleOff` is Off. The wire spells it `-1`, and so does this, because
    /// "the viewer said no subtitles" and "the viewer said nothing" are
    /// different requests and the server answers them differently.
    var subtitleIndex: Int?

    static let subtitleOff = -1
    static let none = PrePlaySelection()

    var isEmpty: Bool { audioIndex == nil && subtitleIndex == nil }

    var queryItems: [URLQueryItem] {
        var items: [URLQueryItem] = []
        if let audioIndex {
            items.append(URLQueryItem(name: "audio", value: String(audioIndex)))
        }
        if let subtitleIndex {
            items.append(URLQueryItem(name: "subtitle", value: String(subtitleIndex)))
        }
        return items
    }
}

struct BookChapter: Codable, Hashable {
    let index: Int
    let title: String
    let startMs: Int
    let endMs: Int
}

struct ReadingRevision: Codable, Hashable {
    let size: Int
    let mtime: Int
}

struct ReadingLocations: Codable, Hashable {
    var fragments: [String]?
    var progression: Double?
    var totalProgression: Double?
    var position: Int?
}

struct ReadingText: Codable, Hashable {
    var before: String?
    var highlight: String?
    var after: String?
}

struct ReadingCinemaLocator: Codable, Hashable {
    var path: [Int]?
    var text: String?
}

struct ReadingLocator: Codable, Hashable {
    let version: Int
    let href: String
    var type: String?
    var title: String?
    var locations: ReadingLocations?
    var text: ReadingText?
    var cinema: ReadingCinemaLocator?
}

struct PublicationMetadata: Codable, Hashable {
    let title: String
    var author: String?
    var identifier: String?
    var language: String?
}

struct PublicationLink: Codable, Hashable {
    let href: String
    let type: String
    var title: String?
}

struct PublicationTocLink: Codable, Hashable {
    let href: String
    let title: String
    var children: [PublicationTocLink]
}

struct PublicationManifest: Codable, Hashable {
    let metadata: PublicationMetadata
    let readingOrder: [PublicationLink]
    let resources: [PublicationLink]
    let toc: [PublicationTocLink]
}

struct PublicationLimits: Codable, Hashable {
    let entries: Int
    let totalUncompressedBytes: Int64
    let resourceBytes: Int64
    let markupBytes: Int64
    let compressionRatio: Int64
    let concurrentResourceReads: Int
    let resourceChunkBytes: Int
}

struct OpenPublicationResponse: Codable, Hashable {
    let sessionId: String
    let resourceBase: String
    let expiresIn: Int
    let fileId: Int
    let revision: ReadingRevision
    let publication: PublicationManifest
    let limits: PublicationLimits
}

struct ReadingState: Codable, Hashable {
    let fileId: Int
    let revision: ReadingRevision
    let locator: ReadingLocator
    let progression: Double
    let completed: Bool
    let updatedAt: Int
}

struct ReadingStateResponse: Codable, Hashable {
    let state: ReadingState?
    let stale: Bool
}

struct PutReadingStateRequest: Codable, Hashable {
    let fileId: Int
    let revision: ReadingRevision
    let locator: ReadingLocator
    let progression: Double
    let completed: Bool
    var recordedAt: Int?
}

struct ItemDetail: Codable {
    let item: Item
    var files: [MediaFile]?
    var children: [Item]?
    var ancestors: [Item]?
    var reading: ReadingState?
    var editions: [Item]? = nil
}

struct Marker: Codable, Hashable {
    let kind: String
    let label: String
    let startMs: Int
    let endMs: Int
    var chapter: Bool?
}

struct SourceSummary: Codable {
    var container: String?
    var videoCodec: String?
    var videoProfile: String?
    var width: Int?
    var height: Int?
    var bitDepth: Int?
    var hdr: String?
    var hdrFormat: String?
    var bitrate: Int?
    var durationMs: Int?
}

struct AudioTrack: Codable, Identifiable {
    var id: Int { index }
    let index: Int
    let codec: String
    var channels: Int?
    var language: String?
    var title: String?
    var `default`: Bool
}

struct SubtitleTrack: Codable, Identifiable {
    var id: Int { index }
    let index: Int
    let codec: String
    var language: String?
    var title: String?
    var `default`: Bool
    var forced: Bool
    /// "Extractable text" — the server's `!is_bitmap_subtitle`. It is *not* the
    /// rendition test: `mov_text` and styled ASS/SSA are text and still cannot
    /// be published as WebVTT renditions. Read `isNativeHLS`, never this.
    var text: Bool
    /// The server's own answer to "can this become a native HLS WebVTT
    /// rendition?", computed by the same classifier the HLS master and the
    /// create-time rejection use. Absent from servers that predate the field.
    var native: Bool?
    /// Optional application-rendered bitmap protocol. It is deliberately
    /// separate from `native`, which continues to mean an HLS text rendition.
    var overlay: String?

    /// Formats the server can expose losslessly enough as an HLS WebVTT
    /// rendition. Styled ASS/SSA remains a burn even though it contains text.
    ///
    /// The server answers this directly now, so ask it rather than re-deriving
    /// it: one classifier means a client cannot request a session the server
    /// will refuse, and the natural recovery from that refusal is the burn
    /// this whole arc exists to avoid. The codec list survives only as the
    /// fallback for a server that omits the field, and it is deliberately the
    /// same list, so such a server degrades exactly as it does today.
    var isNativeHLS: Bool {
        native ?? ["subrip", "srt", "webvtt", "vtt"].contains(codec.lowercased())
    }

    var isPGSOverlay: Bool { overlay == PGSOverlayPolicy.protocolName }
}

struct QualityRung: Codable, Identifiable {
    var id: Int { height }
    let height: Int
    let totalKbps: Int
    let peakKbps: Int
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
    var preserveDolbyVision: Bool?
    /// The audio stream index this plan carries when the caller selected one.
    /// It is already applied to `url`; the HLS transport takes it in the
    /// session-create body instead, which is why the plan repeats it. A client
    /// executes this rather than re-adding its own index (docs/CLIENTS.md §1).
    var audio: Int?
}

/// `/decision`'s answer about a request-local track choice. Present only when
/// the request carried `audio=` or `subtitle=`, so an unselected request keeps
/// the response shape it has always had.
struct DecisionSelection: Codable {
    var audioIndex: Int?
    /// The effective subtitle. `nil` means Off — either because the caller
    /// asked for `-1` or because policy selected none.
    var subtitleIndex: Int?
    /// The selected subtitle is bitmap data with no application-overlay route,
    /// so showing it means drawing it into a video transcode.
    var subtitleRequiresBurnIn: Bool?
    /// The HDR guard refused that burn rather than silently replacing HDR or
    /// Dolby Vision with SDR — so the delivery is unchanged and the subtitle is
    /// *not* on. Reporting this as subtitles-on would be a lie.
    var subtitleBurnInBlockedByHdr: Bool?
}

struct Decision: Codable {
    let fileId: Int
    let method: String          // "direct_play" | "remux" | "transcode"
    let playUrl: String
    var delivery: Delivery?
    var reasons: [String]?
    var transcodeAudio: Bool?
    var preserveDolbyVision: Bool?
    var source: SourceSummary?
    var audio: [AudioTrack]?
    var subtitles: [SubtitleTrack]?
    var selection: DecisionSelection?
    var markers: [Marker]?
    var audioOffsetMs: Int?
    var declaredOffsetMs: Int?
    var ladder: [QualityRung]?
    /// The dynamic range of the bytes this delivery plan would put on the wire
    /// — `"dolby_vision" | "hdr10" | "hlg" | "sdr"`, the same vocabulary as
    /// `SourceSummary.hdr` plus `"sdr"`, so source and delivered compare by
    /// string equality. Optional because an older server does not send it;
    /// absent means "unknown", and the badges stay source-only.
    var deliveredDynamicRange: String?
}

struct HlsStart: Codable {
    let sessionId: String
    let playlistUrl: String
    var durationMs: Int?
    var startSeconds: Double?
    /// Source-timeline timestamp represented by player-local time zero.
    /// Optional so this client remains compatible with older servers.
    var mediaOriginMs: Int?
    var encoder: String?
    var vod: Bool?
    var ladder: [QualityRung]?
    /// The normalized output height this session actually received — for a
    /// bound stall reopen, the persisted one-rung-down answer, repeated
    /// unchanged when the same `request_id` is replayed. Optional so an older
    /// server that never sent it still decodes; `0` is the server's own answer
    /// for a remux of an unprobed source, and both spellings of "unknown" are
    /// read the same way by `StallReopenBudget`.
    var height: Int?
    /// What the session that was actually created delivers. It overrides the
    /// decision's value the moment a session attaches, because a burn or a
    /// manually-picked rung forces a transcode the decision never promised.
    /// Nullable server-side too: a session whose source row could not be read
    /// omits it, and the client keeps whatever it had.
    var deliveredDynamicRange: String?
}

/// Live HLS telemetry used by the web client's playback-info panel and now by
/// the Apple client too. All changing measurements remain optional because a
/// session has not necessarily produced its first segment when first polled.
struct PlaybackSessionStatus: Codable {
    let id: String
    var targetHeight: Int?
    var encoder: String?
    var speed: Double?
    var recentSpeed: Double?
    var outTimeMs: Int?
    var progressIdleMs: Int?
    var publishedEndMs: Int?
    var fetchedEndMs: Int?
    var fetchedSegment: Int?
    var firstRetainedSegment: Int?
    var playlistShape: String?
    var aheadSeconds: Int?
    var aheadBytes: Int?
    var holdReason: String?
    var resumeBelowSeconds: Int?
    var resumeBelowBytes: Int?
    var deliveredBytes: Int?
    var deliveredBps: Int?
    var deliveredIdleMs: Int?
    var readrate: Double?
    var suspended: Bool?
    var suspendCount: Int?
    var lastRequest: String?
    var idleSeconds: Int?
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
    /// The exact session a typed recovery steps down from. Required together
    /// with `reopenReason`; ordinary seeks and track changes omit both.
    var previousSessionId: String?
    /// Why this create replaces its predecessor. `"stall"` is the only value
    /// the server accepts, and an unknown one is a client error.
    var reopenReason: String?
    var height: Int?
    /// Whether the VIEWER chose Auto quality — a different question from
    /// "does this body carry a `height`". A subtitle burn posts the source
    /// height as a promise about the output with no opinion about the quality
    /// menu, and without this field the server would read that promise as a
    /// sticky manual pick and never step the session down.
    var qualityAuto: Bool?
    var start: Double?
    var audio: Int?
    /// A bitmap or styled-text subtitle the server must draw into the video.
    var subtitleBurn: Int?
    /// True only when the selected plan already delivers SDR. This lets an
    /// HDR source keep a forced bitmap subtitle on an existing tone-map
    /// without authorizing an HDR delivery to be silently downgraded.
    var subtitleBurnSDR: Bool?
    /// Ask for an HLS master containing native WebVTT subtitle renditions.
    var nativeSubtitles: Bool?
    /// The initial native media-selection option. It does not alter video.
    var subtitle: Int?
    /// Copy the source video into HLS untouched (the remux plan).
    var copy: Bool?
    /// With `copy`: re-encode the audio the client can't take.
    var aac: Bool?
    /// Retain a source profile the server already decided this AVPlayer can
    /// decode. Omitted/false remains the safe legacy behavior.
    var preserveDolbyVision: Bool?
}

struct ProgressRequest: Codable {
    let positionMs: Int
    var durationMs: Int?
    var recordedAt: Int? = nil
}
