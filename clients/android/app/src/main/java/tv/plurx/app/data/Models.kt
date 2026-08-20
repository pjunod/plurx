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
    val instance_id: String? = null,
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
    /**
     * Epoch seconds the server first saw this item. The stable ordering key
     * for a merged category, where each share sorts correctly on its own but
     * the combined grid cannot without it.
     */
    val added_at: Long? = null,
    val watch: Watch? = null,
    val rollup: Rollup? = null,
) {
    val isPlayableVideo get() = kind == "movie" || kind == "episode" || kind == "video"
    val isPlayable get() = isPlayableVideo || kind == "audiobook"
    val isBook get() = kind == "book"
    val isAudiobook get() = kind == "audiobook"
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
    /**
     * The container's `hearing_impaired` disposition — the SDH track that also
     * transcribes the door slam. Absent on a library probed before the server
     * learned to read the flag, which is indistinguishable from "not SDH" and
     * is why the detail screen only ever *adds* the marker.
     */
    val hearing_impaired: Boolean = false,
)

/**
 * One half of `playback_defaults` — the server's cold-start answer for a file's
 * audio or subtitle tracks (`PlaybackTrackDefaultDto`, crates/plurxd
 * http/dto.rs). Clients mark [selected_index] and render
 * [preferred_language_status]; they do not fetch admin settings or re-run
 * `select_tracks`. See docs/CLIENTS.md §"Shared track facts".
 */
@Serializable
data class PlaybackTrackDefault(
    /** The policy-selected stream index, or null for none (subtitles Off). */
    val selected_index: Long? = null,
    /** The configured preferred language, as a wire code ("eng"). */
    val preferred_language: String = "",
    /** `selected` | `available` | `missing` | `unknown` | `no_tracks`. */
    val preferred_language_status: String = "",
)

@Serializable
data class PlaybackDefaults(
    val audio: PlaybackTrackDefault = PlaybackTrackDefault(),
    val subtitle: PlaybackTrackDefault = PlaybackTrackDefault(),
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
    /**
     * The server's own track outcome for this file. Null on a server that
     * predates the shared contract — the detail screen then lists the tracks
     * without claiming which one plays, rather than inventing a policy of its
     * own.
     */
    val playback_defaults: PlaybackDefaults? = null,
    val part_offset_ms: Long = 0,
    val chapters: List<BookChapter> = emptyList(),
    val available: Boolean = true,
    val probed: Boolean = true,
    val missing_path: String? = null,
)

@Serializable
data class BookChapter(
    val index: Long,
    val title: String,
    val start_ms: Long,
    val end_ms: Long,
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
    /** Not a bitmap. True for ASS/SSA, which are still unservable as WebVTT. */
    val text: Boolean = true,
    /**
     * The server's own answer to "can this become a native HLS rendition?"
     * (`SubTrackDto.native`, crates/plurxd/src/http/stream.rs). Absent on
     * servers that predate the field — read [isNativeHls], never this.
     */
    val native: Boolean? = null,
    /**
     * Versioned application-overlay contract advertised by the server. Only
     * `pgs-v1` is understood; an absent or future value deliberately keeps the
     * legacy burn/refusal policy.
     */
    val overlay: String? = null,
) {
    /**
     * Route on this, not on [text]. The two questions differ for exactly one
     * family and the difference is expensive: ASS/SSA carry text, so [text] is
     * true, but their authored positioning does not survive WebVTT conversion,
     * the master never advertises them, and `POST …/hls/sessions` answers a
     * native request for one with "the selected subtitle requires burn-in".
     *
     * When the server didn't answer (an older build omits the field) we fall
     * back to the classification this client could already make. `false` would
     * be the safe-looking default and is the wrong one: it would burn every
     * SRT on every 4K remux — the bug this milestone exists to remove — against
     * servers that serve renditions perfectly well. `text` alone would be too
     * eager the other way and ask an old server to serve an ASS rendition, so
     * the styled formats are excluded here. That codec list is the one place
     * this client duplicates the server's classifier, and it is consulted only
     * when the server declined to classify.
     */
    val isNativeHls: Boolean
        get() = native ?: (text && codec.lowercase().trim() !in STYLED_SUBTITLE_CODECS)

    val isPgsOverlay: Boolean
        get() = overlay == "pgs-v1"

    private companion object {
        val STYLED_SUBTITLE_CODECS = setOf("ass", "ssa")
    }
}

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
    /**
     * Remux only: keep Dolby Vision signaling through the copy. Carried on a
     * copy session create so a subtitle selection can't quietly cost the
     * stream its DV layer — the server decided this, the client repeats it.
     */
    val preserve_dolby_vision: Boolean = false,
    /**
     * The audio stream index this plan carries, when the caller selected one.
     * It is **already applied** to [url] (`stream.mp4?audio=<index>`); it is
     * repeated here because the HLS transport takes it in the session-create
     * body instead of a query. Execute it as given — a client that re-derives
     * the index, or reuses a bare `/stream.mp4`, gets the server's language
     * default rather than the viewer's choice.
     */
    val audio: Long? = null,
)

/**
 * `DecisionResponse.selection` — the effective request-local choices, present
 * only when the request carried `audio=` or `subtitle=`. This is what lets the
 * detail screen price a pre-play subtitle *before* playback starts instead of
 * re-deriving burn-in policy from the codec.
 */
@Serializable
data class DecisionSelection(
    val audio_index: Long? = null,
    /** Null means Off — either the request's `-1`, or no track selected. */
    val subtitle_index: Long? = null,
    /**
     * The selected subtitle is bitmap data with no enabled overlay route, so
     * showing it costs a video re-encode. Text tracks stay false here and are
     * priced by [SubTrack.isNativeHls] instead.
     */
    val subtitle_requires_burn_in: Boolean = false,
    /**
     * The HDR guard refused that burn rather than replacing HDR with SDR, so
     * the current delivery was kept and the subtitle is *not* being shown.
     */
    val subtitle_burn_in_blocked_by_hdr: Boolean = false,
)

/**
 * `DecisionResponse.source` — what the file actually is. `height` is the one
 * the player owes back to the server whenever its own height is a *promise*
 * rather than a transcode rung (a burn, or Quality = Original): §3.2 of
 * docs/CLIENTS-REMEDIATION-PLAN.md, and `hls.rs`'s "the source's own height
 * is the Original/forced-burn promise" escape from ladder snapping.
 */
@Serializable
data class SourceSummary(
    val container: String? = null,
    val video_codec: String? = null,
    val video_profile: String? = null,
    val width: Long? = null,
    val height: Long? = null,
    val bit_depth: Long? = null,
    val hdr: String? = null,
    val hdr_format: String? = null,
    val bitrate: Long? = null,
    val duration_ms: Long? = null,
)

/**
 * One advertised rung of the server's quality ladder (`transcode::Rung`), top
 * rung first and already filtered to what the source can feed — so the client's
 * quality menu stops hardcoding heights the source cannot reach.
 */
@Serializable
data class Rung(
    val height: Int,
    val total_kbps: Int = 0,
    val peak_kbps: Int = 0,
)

@Serializable
data class Decision(
    val file_id: Long,
    val method: String,
    val play_url: String,
    val delivery: Delivery? = null,
    val source: SourceSummary? = null,
    val reasons: List<String> = emptyList(),
    val transcode_audio: Boolean = false,
    /** The source DV metadata survives this decision, including direct play. */
    val preserve_dolby_vision: Boolean = false,
    val audio: List<AudioTrack> = emptyList(),
    val subtitles: List<SubTrack> = emptyList(),
    /** Present only for a selection-aware request; see [DecisionSelection]. */
    val selection: DecisionSelection? = null,
    val markers: List<Marker> = emptyList(),
    val ladder: List<Rung> = emptyList(),
    val audio_offset_ms: Long = 0,
    val declared_offset_ms: Long? = null,
    /**
     * The dynamic range of the bytes this delivery plan would put on the wire —
     * `"dolby_vision" | "hdr10" | "hlg" | "sdr"`, the source's own vocabulary
     * plus `"sdr"`, so a client compares source against delivered with string
     * equality. Absent on a server that predates it; the badge then falls back
     * to describing the source alone.
     */
    val delivered_dynamic_range: String? = null,
)

@Serializable
data class HlsStart(
    val session_id: String,
    val playlist_url: String,
    /**
     * The normalized height the server resolved for this session. The
     * authoritative answer a stall reopen reads to detect the floor.
     */
    val height: Int? = null,
    val duration_ms: Long? = null,
    val start_seconds: Double = 0.0,
    /**
     * Source timestamp represented by player-local time zero. Copy sessions
     * can begin at the keyframe preceding [start_seconds], so this optional
     * origin is the authoritative timeline anchor on newer servers.
     */
    val media_origin_ms: Long? = null,
    val encoder: String? = null,
    /**
     * The whole stream is already on disk (a pre-transcode cache hit), so it
     * behaves like direct play: `start_seconds` is 0 and the client seeks.
     * Defaults false because a server that omits it has no such cache to
     * promise — §5.5 consumes this.
     */
    val vod: Boolean = false,
    /** The rungs this source can feed, top first — §5.6's quality menu. */
    val ladder: List<Rung> = emptyList(),
    /**
     * What *this session* delivers, which overrides the decision's answer the
     * moment the session attaches: a burn or a manually-picked rung forces a
     * transcode the decision never promised. Null when the server could not
     * read the source mid-request — the client then keeps what it had.
     */
    val delivered_dynamic_range: String? = null,
)

/**
 * Why a session is being reopened — typed so the server can tell a stall
 * downgrade from an ordinary seek. See docs/ADAPTIVE-QUALITY.md §"Native
 * stall-reopen server boundary".
 */
@Serializable
enum class ReopenReason {
    @kotlinx.serialization.SerialName("stall") Stall,
}

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
    /**
     * Draw a subtitle into the frames. Only for tracks with no text to send —
     * a bitmap (PGS/VobSub) or a styled one the server won't serve as WebVTT.
     */
    val subtitle_burn: Int? = null,
    /**
     * Advertise the source's WebVTT-convertible tracks as HLS renditions in
     * the master playlist. Changes only HLS metadata, never the video recipe,
     * so it composes with `copy` — which is the entire point of §5.4.
     */
    val native_subtitles: Boolean? = null,
    /**
     * The initially selected native rendition: the **absolute** subtitle
     * stream index (`SubTrack.index`), not an ordinal among native tracks.
     * The server rejects a non-native index here with a 400.
     */
    val subtitle: Int? = null,
    /** Manual A/V correction for this playback only; positive delays audio. */
    val audio_offset_ms: Long? = null,
    val copy: Boolean? = null,
    val aac: Boolean? = null,
    /** With `copy`: the decision said this client can take the source's DV. */
    val preserve_dolby_vision: Boolean? = null,
    /**
     * The session this one replaces — set only on a stall-driven reopen so
     * the server can step the resolved rung down. Omitted on ordinary seeks
     * and track switches.
     */
    val previous_session_id: String? = null,
    /**
     * Why this session replaces its predecessor. Typed cause so the server
     * distinguishes a stall downgrade from an ordinary restart. Omitted on
     * ordinary seeks and track switches.
     */
    val reopen_reason: ReopenReason? = null,
    /**
     * When true, signals this session's height is a promise (the viewer is on
     * Auto), so the server must not treat the posted height as a sticky manual
     * pick. Every Auto viewer that sends a promise-height — a burn or
     * Original — must carry this flag, or the server can never step that
     * session down.
     */
    val quality_auto: Boolean? = null,
)

@Serializable
data class ProgressReq(
    val position_ms: Long,
    val duration_ms: Long? = null,
    val recorded_at: Long? = null,
)

@Serializable
data class OfflineQualityOption(
    val height: Int,
    val label: String,
    val estimated_bytes: Long,
    val reserved_bytes: Long,
)

@Serializable
data class OfflineAudioOption(
    val index: Long,
    val codec: String,
    val channels: Int? = null,
    val language: String? = null,
    val title: String? = null,
    val default: Boolean = false,
)

@Serializable
data class OfflineSubtitleOption(
    val index: Long,
    val codec: String,
    val language: String? = null,
    val title: String? = null,
    val default: Boolean = false,
    val forced: Boolean = false,
    val offline_mode: String,
)

@Serializable
data class OfflineOptions(
    val file_id: Long,
    val qualities: List<OfflineQualityOption> = emptyList(),
    val audio: List<OfflineAudioOption> = emptyList(),
    val subtitles: List<OfflineSubtitleOption> = emptyList(),
    val recommended_audio_index: Long? = null,
    val recommended_subtitle_index: Long? = null,
)

@Serializable
data class CreateOfflinePackageReq(
    val request_id: String,
    val height: Int,
    val audio_index: Long? = null,
    val subtitle_index: Long? = null,
)

@Serializable
data class OfflinePackageOutput(
    val height: Int,
    val video_codec: String,
    val audio_codec: String,
    val dynamic_range: String,
    val subtitle_mode: String,
)

@Serializable
data class OfflinePackageFailure(val code: String, val message: String)

@Serializable
data class OfflinePackageStatus(
    val id: String,
    val state: String,
    val phase: String,
    val status_url: String,
    val progress: Double? = null,
    val bytes_ready: Long = 0,
    val estimated_bytes: Long,
    val actual_bytes: Long? = null,
    val duration_ms: Long? = null,
    val output: OfflinePackageOutput,
    val error: OfflinePackageFailure? = null,
)

@Serializable
data class OfflineLeaseReq(val token: String)

@Serializable
data class OfflineLease(
    val manifest_url: String,
    val expires_at: Long,
    val bytes: Long,
    val duration_ms: Long,
)

@Serializable
data class MutationResult(val ok: Boolean = true, val updated: Int = 0)
