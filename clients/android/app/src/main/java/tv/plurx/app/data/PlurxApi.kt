package tv.plurx.app.data

import retrofit2.http.Body
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
        @Query("sort") sort: String = "title",
    ): Page

    @GET("hubs")
    suspend fun hubs(): Hubs

    @GET("items/{id}")
    suspend fun item(@Path("id") id: Long): ItemDetail

    /** The runtime caps map (vcodec/acodec/container/hdr/force) rides as query params. */
    @GET("files/{id}/decision")
    suspend fun decision(
        @Path("id") id: Long,
        @QueryMap caps: Map<String, String>,
    ): Decision

    @GET("files/{id}/hls/start")
    suspend fun hlsStart(
        @Path("id") id: Long,
        @Query("height") height: Int,
        @Query("start") start: Double,
        @QueryMap caps: Map<String, String>,
    ): HlsStart

    @POST("items/{id}/progress")
    suspend fun progress(@Path("id") id: Long, @Body body: ProgressReq)
}
