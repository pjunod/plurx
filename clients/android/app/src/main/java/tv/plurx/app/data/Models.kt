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
)

@Serializable
data class Item(
    val id: Long,
    val kind: String,
    val title: String,
    val year: Int? = null,
    val overview: String? = null,
    val poster: String? = null,
    val backdrop: String? = null,
    val season_number: Int? = null,
    val episode_number: Int? = null,
    val show_title: String? = null,
    val runtime_ms: Long? = null,
    val watch: Watch? = null,
) {
    val isMovieOrEpisode get() = kind == "movie" || kind == "episode"
}

@Serializable
data class Hubs(
    val continue_watching: List<Item> = emptyList(),
    val next_up: List<Item> = emptyList(),
    val recently_added: List<Item> = emptyList(),
)

@Serializable
data class Page(val items: List<Item> = emptyList(), val total: Int = 0)

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
    val width: Long? = null,
    val height: Long? = null,
    val bit_depth: Long? = null,
    val hdr: String? = null,
    val hdr_format: String? = null,
    val bitrate: Long? = null,
    val audio_streams: List<AudioStream> = emptyList(),
    val subtitle_streams: List<SubtitleStream> = emptyList(),
    val available: Boolean = true,
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

@Serializable
data class Decision(
    val file_id: Long,
    val method: String,
    val play_url: String,
    val reasons: List<String> = emptyList(),
    val transcode_audio: Boolean = false,
    val audio: List<AudioTrack> = emptyList(),
    val subtitles: List<SubTrack> = emptyList(),
    val markers: List<Marker> = emptyList(),
    val audio_offset_ms: Long = 0,
)

@Serializable
data class HlsStart(
    val session_id: String,
    val playlist_url: String,
    val duration_ms: Long? = null,
    val start_seconds: Double = 0.0,
    val encoder: String? = null,
)

@Serializable
data class ProgressReq(val position_ms: Long, val duration_ms: Long? = null)
