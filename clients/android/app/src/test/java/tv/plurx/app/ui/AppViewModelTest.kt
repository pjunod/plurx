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
    fun qrCodesAcceptServerAddressesAndRejectUnrelatedPayloads() {
        assertEquals(
            "http://192.168.4.10:32400",
            connectionOriginFromQr("http://192.168.4.10:32400/"),
        )
        assertEquals(
            "http://media-box:32400",
            connectionOriginFromQr(
                "plurx://connect?origin=http%3A%2F%2Fmedia-box%3A32400",
            ),
        )
        assertEquals(null, connectionOriginFromQr("https://example.com/not-a-server-page"))
        assertEquals(null, connectionOriginFromQr("wifi password"))
    }

    @Test
    fun savedServerIdentityMatchesExactlyAndMigratesLegacyBonjourHosts() {
        val instanceId = "4f2cfb82-9162-4be0-a8bb-0123456789ab"
        assertEquals(true, matchesSavedServer(instanceId, instanceId, "http://old:32400"))
        assertEquals(
            true,
            matchesSavedServer(
                instanceId,
                null,
                "http://plurx-4f2cfb829162.local:32400",
            ),
        )
        assertEquals(
            false,
            matchesSavedServer(
                "different-server",
                instanceId,
                "http://plurx-4f2cfb829162.local:32400",
            ),
        )
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
