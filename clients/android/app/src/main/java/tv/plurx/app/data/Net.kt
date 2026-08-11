package tv.plurx.app.data

import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.okhttp.OkHttpDataSource
import kotlinx.serialization.json.Json
import okhttp3.Call
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import retrofit2.Retrofit
import retrofit2.converter.kotlinx.serialization.asConverterFactory
import java.util.concurrent.TimeUnit

/**
 * One shared OkHttpClient (adds the bearer token from [Session] on every
 * request) drives Retrofit, Coil image loading, and Media3 playback — so
 * artwork and video streams authenticate exactly like the API. Retrofit goes
 * through thin derivatives of it that add a call deadline — a 30 s one for
 * ordinary JSON and a 180 s one for playback preparation; see [clientFor].
 */
object Net {
    val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }

    val client: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(20, TimeUnit.SECONDS)
        .readTimeout(60, TimeUnit.SECONDS)
        .addInterceptor { chain ->
            val token = Session.token
            val req = chain.request()
            // `/server` is public and identifies rediscovered candidates.
            // Never leak a saved bearer token while probing the LAN.
            val publicIdentityRequest = req.url.encodedPath == "/api/v1/server"
            val out = if (token != null && !publicIdentityRequest) {
                req.newBuilder().header("Authorization", "Bearer $token").build()
            } else {
                req
            }
            chain.proceed(out)
        }
        .build()

    /**
     * The JSON path's deadline, and only the JSON path's.
     *
     * A blackholing host — wrong subnet, or a firewall that drops instead of
     * refusing — never fails, so without a `callTimeout` a screen spins until
     * the viewer gives up and there is no failure to classify. 30 s bounds it.
     *
     * This is deliberately a *separate* client. The same `callTimeout` on
     * [client] would apply to `OkHttpDataSource.Factory` and the offline
     * downloader, aborting a long media segment read or a 700 MB offline
     * package transfer at 30 seconds. `newBuilder()` shares the connection
     * pool and dispatcher, so the split costs nothing.
     */
    private val apiClient: OkHttpClient = client.newBuilder()
        .callTimeout(API_CALL_TIMEOUT_SECONDS, TimeUnit.SECONDS)
        .build()

    /**
     * Playback preparation is not an ordinary JSON call, however much it looks
     * like one.
     *
     * `/decision` reaches `markers_for` server-side, which can fall through to
     * a live `ffprobe -show_chapters` with no timeout of its own, behind an
     * availability stat that may be sitting on a spun-down NAS; opening an HLS
     * session spawns an encoder. Before the API deadline existed these were
     * governed by the 60 s read timeout. Giving them the 30 s API deadline
     * *shortened* their budget and turned the first play of a large remux into
     * "No answer from {server}." for a file that plays fine on the second
     * press — so they get their own, longer one, matching what web (120 s) and
     * Apple (180 s) already give the same two routes.
     */
    private val playbackPreparationClient: OkHttpClient = client.newBuilder()
        .callTimeout(PLAYBACK_PREPARATION_TIMEOUT_SECONDS, TimeUnit.SECONDS)
        .build()

    /**
     * Which deadline a request gets. `callTimeout` is a property of the client,
     * not of the call, so an interceptor cannot adjust it — the choice has to
     * be made when the call is created.
     */
    internal fun clientFor(encodedPath: String): OkHttpClient =
        if (isPlaybackPreparation(encodedPath)) playbackPreparationClient else apiClient

    private val callFactory = object : Call.Factory {
        override fun newCall(request: Request): Call =
            clientFor(request.url.encodedPath).newCall(request)
    }

    private val contentType = "application/json".toMediaType()

    /** Build an API bound to a server origin (`http://host:32400`). */
    fun api(origin: String): PlurxApi = retrofit(origin).create(PlurxApi::class.java)

    internal fun retrofit(origin: String): Retrofit =
        Retrofit.Builder()
            .baseUrl("$origin/api/v1/")
            .callFactory(callFactory)
            .addConverterFactory(json.asConverterFactory(contentType))
            .build()

    /** Media3 HTTP data source that carries the same auth header. */
    @UnstableApi
    fun dataSourceFactory(): OkHttpDataSource.Factory =
        OkHttpDataSource.Factory(client)
}

/** docs §3, Android row. */
internal const val API_CALL_TIMEOUT_SECONDS = 30L
internal const val PLAYBACK_PREPARATION_TIMEOUT_SECONDS = 180L

/**
 * Is this one of the two playback-preparation routes?
 *
 * Kept as a function of the path rather than a set of `PlurxApi` methods
 * because the choice has to be made from an `okhttp3.Request`, which is all
 * Retrofit hands a `Call.Factory`. `NetTimeoutTest` pins these strings against
 * the `@GET`/`@POST` annotations on [PlurxApi] so a renamed route cannot
 * silently drop back to the 30 s deadline.
 */
internal fun isPlaybackPreparation(encodedPath: String): Boolean {
    val segments = encodedPath.trim('/').split('/')
    val last = segments.lastOrNull() ?: return false
    // files/{id}/decision
    if (last == "decision") return true
    // files/{id}/hls/sessions
    return last == "sessions" && segments.getOrNull(segments.size - 2) == "hls"
}
