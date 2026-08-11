package tv.plurx.app.data

import okhttp3.Request
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import retrofit2.http.GET
import retrofit2.http.POST
import java.util.concurrent.TimeUnit

/**
 * docs §3. The deadlines are asserted through the *real* `Retrofit` instance
 * `Net.api()` is built from, not through a restatement of them, so reverting
 * the call factory to the plain shared client — or handing playback
 * preparation the ordinary API deadline — fails here rather than in the field.
 */
class NetTimeoutTest {

    private val origin = "http://plurx-ab12cd.local:32400"

    private fun callTimeoutMillisFor(path: String): Long {
        val factory = Net.retrofit(origin).callFactory()
        val call = factory.newCall(Request.Builder().url("$origin$path").build())
        return TimeUnit.NANOSECONDS.toMillis(call.timeout().timeoutNanos())
    }

    @Test
    fun ordinaryJsonCallsCarryTheApiDeadline() {
        // A blackholing host never fails; without a deadline the screen spins
        // forever and there is no failure to classify.
        assertEquals(30_000L, callTimeoutMillisFor("/api/v1/hubs"))
        assertEquals(30_000L, callTimeoutMillisFor("/api/v1/libraries"))
        assertEquals(30_000L, callTimeoutMillisFor("/api/v1/auth/login"))
        assertEquals(30_000L, callTimeoutMillisFor("/api/v1/items/42"))
        assertEquals(30_000L, callTimeoutMillisFor("/api/v1/offline/packages/abc"))
    }

    /**
     * `/decision` reaches a server-side `ffprobe -show_chapters` with no
     * timeout of its own, behind an availability stat that may be sitting on a
     * spun-down NAS; opening an HLS session spawns an encoder. The API
     * deadline is *shorter* than the 60 s read timeout these used to have, so
     * giving it to them turned the first play of a large remux into "No answer
     * from {server}." for a file that plays fine on the second press.
     */
    @Test
    fun playbackPreparationKeepsTheLongDeadline() {
        assertEquals(180_000L, callTimeoutMillisFor("/api/v1/files/42/decision"))
        assertEquals(180_000L, callTimeoutMillisFor("/api/v1/files/42/hls/sessions"))
        assertTrue(
            "playback preparation must outlast the ordinary API deadline",
            callTimeoutMillisFor("/api/v1/files/42/decision") >
                callTimeoutMillisFor("/api/v1/hubs"),
        )
    }

    /**
     * The media client must keep *no* call deadline: it is handed to
     * `OkHttpDataSource.Factory` and the offline downloader, where a 30 s cap
     * would abort a long segment read or a 700 MB package mid-transfer.
     */
    @Test
    fun theSharedMediaClientHasNoCallDeadline() {
        assertEquals(0, Net.client.callTimeoutMillis)
        assertEquals(60_000, Net.client.readTimeoutMillis)
        assertEquals(20_000, Net.client.connectTimeoutMillis)
    }

    /**
     * The route predicate is matched against the annotations on [PlurxApi]
     * rather than against a copy of the paths, so renaming a route cannot
     * silently drop it back to the 30 s deadline — the same trick
     * `ModelContractTest` uses for the offline routes.
     */
    @Test
    fun theSlowRoutesAreExactlyTheOnesDeclaredOnPlurxApi() {
        val decision = PlurxApi::class.java.declaredMethods.single { it.name == "decision" }
        val session = PlurxApi::class.java.declaredMethods.single { it.name == "createHlsSession" }
        val decisionPath = requireNotNull(decision.getAnnotation(GET::class.java)).value
        val sessionPath = requireNotNull(session.getAnnotation(POST::class.java)).value

        assertTrue(decisionPath, isPlaybackPreparation("/api/v1/$decisionPath"))
        assertTrue(sessionPath, isPlaybackPreparation("/api/v1/$sessionPath"))
        assertEquals(180_000L, callTimeoutMillisFor("/api/v1/${decisionPath.replace("{id}", "7")}"))
        assertEquals(180_000L, callTimeoutMillisFor("/api/v1/${sessionPath.replace("{id}", "7")}"))
    }

    @Test
    fun nothingElseIsMistakenForPlaybackPreparation() {
        assertFalse(isPlaybackPreparation("/api/v1/hubs"))
        assertFalse(isPlaybackPreparation("/api/v1/files/7/offline-options"))
        assertFalse(isPlaybackPreparation("/api/v1/hls/abc"))
        assertFalse(isPlaybackPreparation("/api/v1/sessions"))
        assertFalse(isPlaybackPreparation(""))
        assertTrue(isPlaybackPreparation("/api/v1/files/7/decision"))
    }
}
