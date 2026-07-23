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
            val out = if (token != null) {
                req.newBuilder().header("Authorization", "Bearer $token").build()
            } else {
                req
            }
            chain.proceed(out)
        }
        .build()

    private val contentType = "application/json".toMediaType()

    /** Build an API bound to a server origin (`http://host:32600`). */
    fun api(origin: String): PlurxApi =
        Retrofit.Builder()
            .baseUrl("$origin/api/v1/")
            .client(client)
            .addConverterFactory(json.asConverterFactory(contentType))
            .build()
            .create(PlurxApi::class.java)

    /** Media3 HTTP data source that carries the same auth header. */
    @UnstableApi
    fun dataSourceFactory(): OkHttpDataSource.Factory =
        OkHttpDataSource.Factory(client)
}
