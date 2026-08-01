package tv.plurx.app.ui

import java.io.IOException
import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Test
import retrofit2.HttpException
import retrofit2.Response

class AppViewModelTest {
    @Test
    fun manualOriginsUseThePlurxPortWhenHttpHasNone() {
        assertEquals("http://192.168.1.20:32400", normalizeOrigin("192.168.1.20"))
        assertEquals("http://media-box:32400", normalizeOrigin("media-box"))
        assertEquals("http://media-box:32400", normalizeOrigin("http://media-box"))
    }

    @Test
    fun manualOriginsPreserveExplicitPortsAndHttpsDefaults() {
        assertEquals("http://media-box:32500", normalizeOrigin("media-box:32500"))
        assertEquals("https://media.example.test", normalizeOrigin("https://media.example.test/"))
        assertEquals("", normalizeOrigin("   "))
    }

    @Test
    fun savedSessionRetriesWhenTvNetworkingIsStillWaking() = runBlocking {
        var calls = 0

        val result = validateSavedSession(
            waitBeforeRetry = {},
            request = {
                calls++
                if (calls == 1) throw IOException("network is not ready")
                "paul"
            },
        )

        assertEquals(SavedSessionValidation.Authenticated("paul"), result)
        assertEquals(2, calls)
    }

    @Test
    fun unavailableServerNeverInvalidatesSavedCredentials() = runBlocking {
        val result = validateSavedSession(
            attempts = 2,
            waitBeforeRetry = {},
            request = { throw IOException("server is waking") },
        )

        assertEquals(SavedSessionValidation.ServerUnavailable, result)
    }

    @Test
    fun explicitUnauthorizedResponseRequiresLoginWithoutRetrying() = runBlocking {
        var calls = 0

        val result = validateSavedSession(
            waitBeforeRetry = {},
            request = {
                calls++
                val body = "{}".toResponseBody("application/json".toMediaType())
                throw HttpException(Response.error<Unit>(401, body))
            },
        )

        assertEquals(SavedSessionValidation.InvalidToken, result)
        assertEquals(1, calls)
    }
}
