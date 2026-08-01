package tv.plurx.app.data

import retrofit2.http.Body
import retrofit2.http.DELETE
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.Path
import retrofit2.http.Query
import retrofit2.http.QueryMap

/**
 * plurx native API (`/api/v1`). Base URL is `<origin>/api/v1/`, so paths are
 * relative. The bearer token is added by an OkHttp interceptor (see [Net]).
 */
interface PlurxApi {
    @GET("server")
    suspend fun server(): Server

    @POST("auth/login")
    suspend fun login(@Body body: LoginReq): LoginResp

    @GET("me")
    suspend fun me(): User

    @GET("libraries")
    suspend fun libraries(): List<Library>

    @GET("libraries/{id}/items")
    suspend fun libraryItems(
        @Path("id") id: Long,
        @Query("limit") limit: Int = 200,
        @Query("offset") offset: Int = 0,
        @Query("sort") sort: String = "title",
    ): Page

    @GET("hubs")
    suspend fun hubs(): Hubs

    @GET("items/{id}")
    suspend fun item(@Path("id") id: Long): ItemDetail

    @GET("search")
    suspend fun search(@Query("q") query: String, @Query("limit") limit: Int = 200): SearchResponse

    @POST("items/{id}/scrobble")
    suspend fun markWatched(@Path("id") id: Long): MutationResult

    @POST("items/{id}/unscrobble")
    suspend fun markUnwatched(@Path("id") id: Long): MutationResult

    /** The runtime caps map (vcodec/acodec/container/hdr/force) rides as query params. */
    @GET("files/{id}/decision")
    suspend fun decision(
        @Path("id") id: Long,
        @QueryMap caps: Map<String, String>,
    ): Decision

    /**
     * POST rather than the deprecated GET bridge: creating a session spawns a
     * process and kills its predecessor, and anything entitled to replay a
     * GET could spawn a second encoder.
     */
    @POST("files/{id}/hls/sessions")
    suspend fun createHlsSession(@Path("id") id: Long, @Body body: CreateSessionReq): HlsStart

    /**
     * Release a session the moment playback ends, instead of leaving its
     * encoder to the server's idle reaper (over a minute of a hardware slot
     * held for nobody). Idempotent and capability-authed server-side.
     */
    @DELETE("hls/{session}")
    suspend fun endHlsSession(@Path("session") session: String)

    @POST("items/{id}/progress")
    suspend fun progress(@Path("id") id: Long, @Body body: ProgressReq)

}
