package tv.plurx.app.data

import okhttp3.ResponseBody
import retrofit2.Response
import retrofit2.http.Body
import retrofit2.http.DELETE
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.PUT
import retrofit2.http.Path
import retrofit2.http.Query
import retrofit2.http.QueryMap
import retrofit2.http.Streaming

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

    @GET("items/{id}/reading-state")
    suspend fun readingState(
        @Path("id") id: Long,
        @Query("file_id") fileId: Long,
    ): ReadingStateResponse

    @PUT("items/{id}/reading-state")
    suspend fun putReadingState(
        @Path("id") id: Long,
        @Body body: PutReadingStateRequest,
    ): ReadingState

    @DELETE("items/{id}/reading-state")
    suspend fun deleteReadingState(
        @Path("id") id: Long,
        @Query("file_id") fileId: Long,
    )

    @POST("files/{id}/publication")
    suspend fun openPublication(@Path("id") id: Long): OpenPublicationResponse

    @DELETE("publication/{session}")
    suspend fun closePublication(@Path("session") session: String)

    @Streaming
    @GET("files/{id}/content")
    suspend fun bookContent(@Path("id") id: Long): Response<ResponseBody>

    @GET("search")
    suspend fun search(@Query("q") query: String, @Query("limit") limit: Int = 200): SearchResponse

    @POST("items/{id}/scrobble")
    suspend fun markWatched(@Path("id") id: Long): MutationResult

    @POST("items/{id}/unscrobble")
    suspend fun markUnwatched(@Path("id") id: Long): MutationResult

    /** Runtime caps (vcodec/vmaxheight/acodec/container/HDR/force) ride as query params. */
    @GET("files/{id}/decision")
    suspend fun decision(
        @Path("id") id: Long,
        @QueryMap caps: Map<String, String>,
    ): Decision

    /**
     * A cold PGS artifact answers 202 with a bounded retry hint; a warm one
     * answers 200 with the versioned source-time manifest. The controller
     * validates both the status payload and the complete manifest identity.
     */
    @GET("files/{id}/subs/{track}/overlay.json")
    suspend fun pgsOverlayManifest(
        @Path("id") id: Long,
        @Path("track") track: Long,
    ): Response<ResponseBody>

    /**
     * Immutable PNG object route. Generation and hash are validated before
     * this method is called, so no server-provided path is interpolated here.
     */
    @GET("files/{id}/subs/{track}/overlay/{generation}/objects/{hash}.png")
    suspend fun pgsOverlayObject(
        @Path("id") id: Long,
        @Path("track") track: Long,
        @Path("generation") generation: String,
        @Path("hash") hash: String,
    ): Response<ResponseBody>

    /**
     * POST rather than the deprecated GET bridge: creating a session spawns a
     * process and kills its predecessor, and anything entitled to replay a
     * GET could spawn a second encoder.
     */
    @POST("files/{id}/hls/sessions")
    suspend fun createHlsSession(@Path("id") id: Long, @Body body: CreateSessionReq): HlsStart

    @GET("hls/{session}/status")
    suspend fun hlsSessionStatus(@Path("session") session: String): PlaybackSessionStatus

    /**
     * Release a session the moment playback ends, instead of leaving its
     * encoder to the server's idle reaper (over a minute of a hardware slot
     * held for nobody). Idempotent and capability-authed server-side.
     */
    @DELETE("hls/{session}")
    suspend fun endHlsSession(@Path("session") session: String)

    @POST("items/{id}/progress")
    suspend fun progress(@Path("id") id: Long, @Body body: ProgressReq)

    @GET("files/{id}/offline-options")
    suspend fun offlineOptions(
        @Path("id") id: Long,
        @Query("audio_lang") audioLanguage: String,
        @Query("subtitle_lang") subtitleLanguage: String,
    ): OfflineOptions

    @POST("files/{id}/offline-packages")
    suspend fun createOfflinePackage(
        @Path("id") id: Long,
        @Body body: CreateOfflinePackageReq,
    ): OfflinePackageStatus

    @GET("offline/packages/{id}")
    suspend fun offlinePackage(@Path("id") id: String): OfflinePackageStatus

    @PUT("offline/packages/{id}/lease")
    suspend fun putOfflineLease(
        @Path("id") id: String,
        @Body body: OfflineLeaseReq,
    ): OfflineLease

    @DELETE("offline/packages/{id}")
    suspend fun deleteOfflinePackage(@Path("id") id: String)

    @POST("offline/packages/{id}/complete")
    suspend fun completeOfflinePackage(@Path("id") id: String)

}
