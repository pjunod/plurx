package tv.plurx.app.ui

import java.io.IOException
import java.net.UnknownHostException
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Test
import retrofit2.HttpException
import retrofit2.Response
import tv.plurx.app.data.ConnectionFailure
import tv.plurx.app.data.copyFor

class AppViewModelTest {
    @Test
    fun onlyTheResumeControlAuthorizesAStoppedOfflineTransfer() {
        assertEquals(false, OfflineResumeTrigger.Lifecycle.explicitUserAction)
        assertEquals(true, OfflineResumeTrigger.UserControl.explicitUserAction)
    }

    @Test
    fun offlineNetworkRecoveryCommitsBeforeAsyncPreferencesPublish() {
        val order = mutableListOf<String>()
        applyOfflineNetworkChange(
            persistRecovery = { order += "recovery" },
            publishPreferences = { order += "preferences" },
        )
        assertEquals(listOf("recovery", "preferences"), order)
    }

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

    /**
     * docs §4. The old behaviour was `catch (_: Exception) { "Wrong username
     * or password" }`, so a viewer whose server was merely off retyped a
     * correct password until they gave up.
     *
     * `AppViewModel` is an `AndroidViewModel` and cannot be constructed on the
     * JVM, so the rule lives in `signInFailureMessage` — a pure function of the
     * throwable, the network state and the server label, exactly like
     * `validateSavedSession` above.
     */
    @Test
    fun signInBlamesCredentialsOnlyForAnAuthResponse() {
        val body = "{}".toResponseBody("application/json".toMediaType())
        assertEquals(
            CREDENTIALS_REJECTED,
            signInFailureMessage(
                HttpException(Response.error<Unit>(401, body)),
                hasNetwork = true,
                server = "Living Room",
            ),
        )
        assertEquals(
            CREDENTIALS_REJECTED,
            signInFailureMessage(
                HttpException(Response.error<Unit>(403, body)),
                hasNetwork = true,
                server = "Living Room",
            ),
        )
    }

    @Test
    fun signInRendersTheConnectivityClassForATransportFailure() {
        assertEquals(
            copyFor(ConnectionFailure.UNKNOWN_HOST, "Living Room").short,
            signInFailureMessage(
                UnknownHostException("Unable to resolve host \"plurx-ab12cd.local\""),
                hasNetwork = true,
                server = "Living Room",
            ),
        )
        assertEquals(
            copyFor(ConnectionFailure.OFFLINE, "Living Room").short,
            signInFailureMessage(
                IOException("network is unreachable"),
                hasNetwork = false,
                server = "Living Room",
            ),
        )
        assertEquals(
            copyFor(ConnectionFailure.SERVER_ERROR, "Living Room").short,
            signInFailureMessage(
                HttpException(
                    Response.error<Unit>(500, "{}".toResponseBody("application/json".toMediaType())),
                ),
                hasNetwork = true,
                server = "Living Room",
            ),
        )
        // The negative: never the credentials sentence, never the native text.
        val transport = signInFailureMessage(
            UnknownHostException("Unable to resolve host \"plurx-ab12cd.local\""),
            hasNetwork = true,
            server = "Living Room",
        )
        assertNotEquals(CREDENTIALS_REJECTED, transport)
        assertFalse(transport.orEmpty().contains("Unable to resolve host"))
    }

    /**
     * A sign-in the viewer navigated away from is not a rejected password.
     * `catch (e: Exception)` catches `CancellationException` in Kotlin, so
     * without the guard inside `classify` this landed on the credentials
     * branch — the same accusation this whole change exists to remove, just
     * arriving by a different route.
     */
    @Test
    fun aCancelledSignInAccusesNobodyOfAnything() {
        assertNull(
            signInFailureMessage(
                CancellationException("StandaloneCoroutine was cancelled"),
                hasNetwork = true,
                server = "Living Room",
            ),
        )
    }
}
