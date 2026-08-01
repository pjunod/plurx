package tv.plurx.app.data

import kotlinx.serialization.Serializable

/**
 * Wire models — a subset of plurx's native `/api/v1` JSON (see crates/plurxd
 * http/dto.rs). Every optional field has a default so an older/newer server
 * that omits or adds one still deserializes (the client sets
 * `ignoreUnknownKeys`).
 */

@Serializable
data class Server(
    val setup_required: Boolean = false,
    val name: String? = null,
    val version: String? = null,
)

@Serializable
data class LoginReq(
    val username: String,
    val password: String,
    val device: String = "Android",
)

@Serializable
data class User(
    val id: Long,
    val username: String,
    val is_admin: Boolean = false,
)

@Serializable
data class LoginResp(val token: String, val user: User)

@Serializable
data class Library(
    val id: Long,
    val name: String,
    val kind: String,
    val anime: Boolean = false,
)

@Serializable
data class Watch(
    val position_ms: Long = 0,
    val duration_ms: Long? = null,
    val watched: Boolean = false,
    val updated_at: Long? = null,
)

@Serializable
data class Rollup(
    val leaves: Long = 0,
    val watched: Long = 0,
)

@Serializable
data class Item(
    val id: Long,
    val library_id: Long? = null,
    val kind: String,
    val parent_id: Long? = null,
    val title: String,
    val year: Int? = null,
    val overview: String? = null,
    val poster: String? = null,
    val backdrop: String? = null,
    val season_number: Int? = null,
    val episode_number: Int? = null,
    val air_date: String? = null,
    val recorded_at: String? = null,
    val tags: List<String> = emptyList(),
    val show_title: String? = null,
    val runtime_ms: Long? = null,
    val resolution: Long? = null,
    val child_count: Long? = null,
    val watch: Watch? = null,
    val rollup: Rollup? = null,
) {
    val isPlayableVideo get() = kind == "movie" || kind == "episode" || kind == "video"
}

@Serializable
data class Hubs(
    val continue_watching: List<Item> = emptyList(),
    val next_up: List<Item> = emptyList(),
    val recently_added: List<Item> = emptyList(),
)

@Serializable
data class Page(
    val items: List<Item> = emptyList(),
    val total: Int = 0,
    val offset: Int = 0,
    val limit: Int = 0,
)

@Serializable
data class SearchResponse(val results: List<Item> = emptyList())

@Serializable
data class AudioStream(
    val index: Long? = null,
    val codec: String? = null,
    val channels: Int? = null,
    val language: String? = null,
    val title: String? = null,
    val default: Boolean = false,
)

@Serializable
data class SubtitleStream(
    val index: Long? = null,
    val codec: String? = null,
    val language: String? = null,
    val title: String? = null,
    val default: Boolean = false,
    val forced: Boolean = false,
)

@Serializable
data class MediaFileDto(
    val id: Long,
    val filename: String,
    val size: Long = 0,
    val duration_ms: Long? = null,
    val container: String? = null,
    val video_codec: String? = null,
    val video_profile: String? = null,
    val width: Long? = null,
    val height: Long? = null,
    val bit_depth: Long? = null,
    val hdr: String? = null,
    val hdr_format: String? = null,
    val bitrate: Long? = null,
    val audio_streams: List<AudioStream> = emptyList(),
    val subtitle_streams: List<SubtitleStream> = emptyList(),
    val available: Boolean = true,
    val probed: Boolean = true,
    val missing_path: String? = null,
)

@Serializable
data class ItemDetail(
    val item: Item,
    val files: List<MediaFileDto> = emptyList(),
    val children: List<Item> = emptyList(),
    val ancestors: List<Item> = emptyList(),
)

@Serializable
data class AudioTrack(
    val index: Long,
    val codec: String,
    val channels: Int? = null,
    val language: String? = null,
    val title: String? = null,
    val default: Boolean = false,
)

@Serializable
data class SubTrack(
    val index: Long,
    val codec: String,
    val language: String? = null,
    val title: String? = null,
    val default: Boolean = false,
    val forced: Boolean = false,
    val text: Boolean = true,
)

@Serializable
data class Marker(
    val kind: String,
    val label: String,
    val start_ms: Long,
    val end_ms: Long,
    val chapter: Boolean = false,
)

/**
 * The server-owned execution plan for a verdict (`DecisionResponse.delivery`
 * in crates/plurxd http/stream.rs). The player executes this rather than
 * re-deriving policy from `method` — re-deriving is how this app came to play
 * transcode verdicts through the copy-only progressive path.
 */
@Serializable
data class Delivery(
    val mode: String, // "direct" | "remux" | "transcode"
    val url: String? = null, // direct: the file; remux: progressive fMP4
    val sessions_url: String? = null, // POST target for an HLS session
    val aac: Boolean = false, // remux over HLS: re-encode the audio
)

@Serializable
data class Decision(
    val file_id: Long,
    val method: String,
    val play_url: String,
    val delivery: Delivery? = null,
    val reasons: List<String> = emptyList(),
    val transcode_audio: Boolean = false,
    val audio: List<AudioTrack> = emptyList(),
    val subtitles: List<SubTrack> = emptyList(),
    val markers: List<Marker> = emptyList(),
    val audio_offset_ms: Long = 0,
    val declared_offset_ms: Long? = null,
)

@Serializable
data class HlsStart(
    val session_id: String,
    val playlist_url: String,
    val duration_ms: Long? = null,
    val start_seconds: Double = 0.0,
    val encoder: String? = null,
)

/**
 * Body for `POST /files/{id}/hls/sessions`. `height` stays null on purpose:
 * omitting it selects the server's Auto rung — the rung depends on which
 * encoder wins, and only the create response knows that. (`Net`'s Json has
 * `explicitNulls = false`, so nulls are genuinely absent on the wire.)
 */
@Serializable
data class CreateSessionReq(
    /** Stable for one player instance; supersession is keyed by it. */
    val playback_id: String,
    /** Fresh per attempt: a replayed create recovers the same session. */
    val request_id: String? = null,
    val height: Int? = null,
    val start: Double? = null,
    val audio: Int? = null,
    val subtitle_burn: Int? = null,
    /** Manual A/V correction for this playback only; positive delays audio. */
    val audio_offset_ms: Long? = null,
    val copy: Boolean? = null,
    val aac: Boolean? = null,
)

@Serializable
data class ProgressReq(val position_ms: Long, val duration_ms: Long? = null)

@Serializable
data class MutationResult(val ok: Boolean = true, val updated: Int = 0)
