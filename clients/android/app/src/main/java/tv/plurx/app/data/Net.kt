package tv.plurx.app.data

import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.okhttp.OkHttpDataSource
import kotlinx.serialization.json.Json
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import retrofit2.Retrofit
import retrofit2.converter.kotlinx.serialization.asConverterFactory
import java.util.concurrent.TimeUnit

/**
 * One shared OkHttpClient (adds the bearer token from [Session] on every
 * request) drives Retrofit, Coil image loading, and Media3 playback — so
 * artwork and video streams authenticate exactly like the API.
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

    /** Capability URLs already carry their narrow authority. Adding the
     * account bearer would widen that authority and expose it to a surface
     * that deliberately does not need it. */
    val capabilityClient: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(20, TimeUnit.SECONDS)
        .readTimeout(60, TimeUnit.SECONDS)
        .followRedirects(false)
        .followSslRedirects(false)
        .build()

    /** Bind background work to the profile that created it. The app-wide
     * client intentionally follows [Session], but a transfer can outlive a
     * screen and must never inherit the next signed-in profile's bearer. */
    fun profileClient(token: String): OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(20, TimeUnit.SECONDS)
        .readTimeout(60, TimeUnit.SECONDS)
        .followRedirects(false)
        .followSslRedirects(false)
        .addInterceptor { chain ->
            chain.proceed(
                chain.request().newBuilder()
                    .header("Authorization", "Bearer $token")
                    .build(),
            )
        }
        .build()

    private val contentType = "application/json".toMediaType()

    /** Build an API bound to a server origin (`http://host:32400`). */
    fun api(origin: String): PlurxApi =
        api(origin, client)

    fun api(origin: String, httpClient: OkHttpClient): PlurxApi =
        Retrofit.Builder()
            .baseUrl("$origin/api/v1/")
            .client(httpClient)
            .addConverterFactory(json.asConverterFactory(contentType))
            .build()
            .create(PlurxApi::class.java)

    /** Media3 HTTP data source that carries the same auth header. */
    @UnstableApi
    fun dataSourceFactory(): OkHttpDataSource.Factory =
        OkHttpDataSource.Factory(client)
}
