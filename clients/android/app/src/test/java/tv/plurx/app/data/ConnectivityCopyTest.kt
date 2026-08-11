package tv.plurx.app.data

import kotlinx.coroutines.CancellationException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.io.InterruptedIOException
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import javax.net.ssl.SSLHandshakeException
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import retrofit2.HttpException
import retrofit2.Response
import tv.plurx.app.ui.CREDENTIALS_REJECTED

/**
 * `tests/contracts/connectivity-copy.json` is the answer key; `Connectivity.kt`
 * is a transcription of it. This test is what keeps the two from drifting, and
 * it is one of four suites (web, Android, Apple, plurxd) reading that same
 * file — adding a class to the JSON fails all four until every client
 * implements it, which is the point.
 */
class ConnectivityCopyTest {

    private val contract: JsonObject = Json.parseToJsonElement(
        checkNotNull(javaClass.classLoader?.getResource("connectivity-copy.json")) {
            "tests/contracts/connectivity-copy.json is not on the JVM test classpath"
        }.readText(),
    ).jsonObject

    private val classes: JsonObject get() = contract.getValue("classes").jsonObject

    private val serverFallback: String
        get() = contract.getValue("server_fallback").jsonPrimitive.content

    private fun failureFor(id: String): ConnectionFailure =
        checkNotNull(ConnectionFailure.entries.firstOrNull { it.id == id }) {
            "the contract carries the class '$id' and Android has no ConnectionFailure for it"
        }

    private fun contractString(id: String, field: String): String =
        classes.getValue(id).jsonObject.getValue(field).jsonPrimitive.content

    /**
     * The contract owns the credentials sentence for the same reason it owns
     * the classes: it is what a client says *instead of* a class, so a client
     * that words it differently has quietly reintroduced the split docs §4
     * exists to close. Asserting `CREDENTIALS_REJECTED == CREDENTIALS_REJECTED`
     * over in the sign-in test is tautological; this is the assertion that
     * isn't, and it is what makes `"Wrong password"` fail the build.
     */
    @Test
    fun theCredentialsSentenceIsTheContractsWordForItVerbatim() {
        assertEquals(
            contract.getValue("credentials_message").jsonPrimitive.content,
            CREDENTIALS_REJECTED,
        )
        // And it is not a connectivity class wearing a disguise.
        assertTrue(
            ConnectionFailure.entries.none { failure ->
                copyFor(failure, "Living Room").short == CREDENTIALS_REJECTED
            },
        )
    }

    @Test
    fun everyContractClassHasAnAndroidFailure() {
        val contractIds = classes.keys
        assertEquals(contractIds, ConnectionFailure.entries.map { it.id }.toSet())
        // The fallback is load-bearing: without it something falls through to
        // a native string.
        assertTrue("unknown" in contractIds)
    }

    @Test
    fun copyIsByteIdenticalToTheContractForANamedServer() {
        val server = "Living Room"
        for (id in classes.keys) {
            val actual = copyFor(failureFor(id), server)
            assertEquals(id, contractString(id, "title").replace("{server}", server), actual.title)
            assertEquals(id, contractString(id, "detail").replace("{server}", server), actual.detail)
            assertEquals(id, contractString(id, "short").replace("{server}", server), actual.short)
        }
    }

    @Test
    fun serverInterpolationFallsBackFromNameToOriginToTheServer() {
        // A name…
        assertEquals(
            contractString("unknown_host", "title").replace("{server}", "Living Room"),
            copyFor(ConnectionFailure.UNKNOWN_HOST, "Living Room").title,
        )
        // …an origin when there is no name…
        assertEquals(
            contractString("unknown_host", "title").replace("{server}", "http://plurx-ab12cd.local:32400"),
            copyFor(ConnectionFailure.UNKNOWN_HOST, "http://plurx-ab12cd.local:32400").title,
        )
        // …and the contract's own fallback when there is neither. A blank
        // string is "neither" too, or the copy reads "Can't find ."
        for (absent in listOf(null, "", "   ")) {
            val copy = copyFor(ConnectionFailure.UNREACHABLE, absent)
            assertEquals(
                contractString("unreachable", "title").replace("{server}", serverFallback),
                copy.title,
            )
            assertEquals(
                contractString("unreachable", "short").replace("{server}", serverFallback),
                copy.short,
            )
        }
        assertEquals("the server", serverFallback)
    }

    @Test
    fun everyClassCarriesTheContractsActionsWithTheContractsLabels() {
        val labels = contract.getValue("actions").jsonObject
        for (id in classes.keys) {
            val expected = classes.getValue(id).jsonObject.getValue("actions").jsonArray
                .map { it.jsonPrimitive.content }
            val actual = copyFor(failureFor(id), "Living Room").actions
            assertEquals(id, expected, actual.map { it.id })
            actual.forEach { action ->
                assertEquals(action.id, labels.getValue(action.id).jsonPrimitive.content, action.label)
            }
            // Rule `every_error_offers_retry`.
            assertTrue(id, ConnectionAction.Retry in actual)
        }
    }

    @Test
    fun realExceptionInstancesMapToTheDocumentedClasses() {
        assertEquals(
            ConnectionFailure.UNKNOWN_HOST,
            classify(
                UnknownHostException(
                    "Unable to resolve host \"plurx-ab12cd.local\": No address associated with hostname",
                ),
                hasNetwork = true,
            ),
        )
        assertEquals(
            ConnectionFailure.INSECURE,
            classify(SSLHandshakeException("Trust anchor for certification path not found."), true),
        )
        assertEquals(
            ConnectionFailure.TIMEOUT,
            classify(SocketTimeoutException("timeout"), true),
        )
        assertEquals(
            // OkHttp's `callTimeout` — the deadline added to the API client in
            // `Net` — surfaces as a plain InterruptedIOException.
            ConnectionFailure.TIMEOUT,
            classify(InterruptedIOException("timeout"), true),
        )
        assertEquals(
            ConnectionFailure.UNREACHABLE,
            classify(ConnectException("Failed to connect to /192.168.1.10:32400"), true),
        )
        assertEquals(
            ConnectionFailure.UNREACHABLE,
            classify(NoRouteToHostException("No route to host"), true),
        )
        // Any other IOException is `unreachable`, per docs §2.2's last row.
        assertEquals(
            ConnectionFailure.UNREACHABLE,
            classify(IOException("unexpected end of stream"), true),
        )
        assertEquals(ConnectionFailure.SERVER_ERROR, classify(httpException(503), true))
        assertEquals(ConnectionFailure.UNKNOWN, classify(IllegalStateException("not connected"), true))
    }

    @Test
    fun wrappedCausesAreUnwrappedAndTheSpecificCauseWins() {
        assertEquals(
            ConnectionFailure.UNKNOWN_HOST,
            classify(RuntimeException("call failed", UnknownHostException("plurx.local")), true),
        )
        // The outer link is a generic IOException, which on its own is
        // `unreachable`. The certificate underneath it is the real answer.
        assertEquals(
            ConnectionFailure.INSECURE,
            classify(IOException("canceled", SSLHandshakeException("bad certificate")), true),
        )
        assertEquals(
            ConnectionFailure.SERVER_ERROR,
            classify(RuntimeException("wrapped", httpException(500)), true),
        )
    }

    /**
     * `catch (e: Exception)` includes `CancellationException` in Kotlin, so
     * without this guard a screen leaving composition or a keystroke
     * superseding a search painted "Something went wrong" on the way out.
     */
    @Test
    fun cancellationIsNotAFailureToReport() {
        assertNull(classify(CancellationException("StandaloneCoroutine was cancelled"), true))
        assertNull(classify(CancellationException("cancelled"), false))
        // Even wrapped, and even wrapping something that would otherwise
        // classify: the caller said stop before the failure meant anything.
        assertNull(classify(RuntimeException("job", CancellationException("cancelled")), true))
        assertNull(
            classify(
                CancellationException("cancelled").apply {
                    initCause(UnknownHostException("plurx.local"))
                },
                true,
            ),
        )
        assertTrue(isCancellation(CancellationException("cancelled")))
        assertFalse(isCancellation(UnknownHostException("plurx.local")))
    }

    /**
     * [classifyNamedTransport] is [classify] minus its two catch-all rows. It
     * exists so the offline transfer surface — where an `IOException` is as
     * likely to be a full disk as a dropped connection — can decline to guess.
     */
    @Test
    fun theStrictClassifierRefusesToGuessForAnUnnamedFailure() {
        // Named: same answers as `classify`.
        assertEquals(
            ConnectionFailure.UNKNOWN_HOST,
            classifyNamedTransport(UnknownHostException("plurx.local"), true),
        )
        assertEquals(
            ConnectionFailure.SERVER_ERROR,
            classifyNamedTransport(httpException(503), true),
        )
        assertEquals(ConnectionFailure.OFFLINE, classifyNamedTransport(IOException("x"), false))
        // Unnamed: null, where `classify` would have said `unreachable`.
        assertEquals(ConnectionFailure.UNREACHABLE, classify(IOException("No space left on device"), true))
        assertNull(classifyNamedTransport(IOException("No space left on device"), true))
        assertNull(classifyNamedTransport(IllegalStateException("nope"), true))
        assertNull(classifyNamedTransport(CancellationException("cancelled"), true))
    }

    /**
     * `cached_content_wins`, as the four screens share it. A failure with
     * content behind it may never replace that content.
     */
    @Test
    fun aFailureOverExistingContentIsABannerAndNeverAnErrorState() {
        assertEquals(
            ConnectionSurface.None,
            connectionSurfaceFor(failure = null, hasContent = true),
        )
        assertEquals(
            ConnectionSurface.None,
            connectionSurfaceFor(failure = null, hasContent = false),
        )
        assertEquals(
            ConnectionSurface.Banner,
            connectionSurfaceFor(ConnectionFailure.UNREACHABLE, hasContent = true),
        )
        assertEquals(
            ConnectionSurface.Full,
            connectionSurfaceFor(ConnectionFailure.UNREACHABLE, hasContent = false),
        )
        // Every class, not just the convenient one.
        for (failure in ConnectionFailure.entries) {
            assertEquals(failure.id, ConnectionSurface.Banner, connectionSurfaceFor(failure, true))
            assertEquals(failure.id, ConnectionSurface.Full, connectionSurfaceFor(failure, false))
        }
    }

    /**
     * The decision inside `hasValidatedNetwork`, which cannot itself run on the
     * JVM. `ConnectivityReadTest` (androidTest) covers the framework read.
     */
    @Test
    fun onlyAValidatedInternetNetworkCountsAsOnline() {
        assertTrue(networkIsUsable(hasCapabilities = true, hasInternet = true, isValidated = true))
        // An interface that is up but has not passed the system's connectivity
        // probe is what a viewer means by "the wifi isn't working".
        assertFalse(networkIsUsable(hasCapabilities = true, hasInternet = true, isValidated = false))
        assertFalse(networkIsUsable(hasCapabilities = true, hasInternet = false, isValidated = true))
        assertFalse(networkIsUsable(hasCapabilities = false, hasInternet = false, isValidated = false))
    }

    @Test
    fun authenticationResponsesAreNotAConnectivityClass() {
        // Null keeps the caller on its existing auth path — see docs §4 and
        // the sign-in rule in `AppViewModelTest`.
        assertNull(classify(httpException(401), true))
        assertNull(classify(httpException(403), true))
        assertNull(classify(RuntimeException("wrapped", httpException(401)), true))
        // Not every 4xx is an auth answer.
        assertEquals(ConnectionFailure.UNKNOWN, classify(httpException(404), true))
    }

    @Test
    fun noValidatedNetworkForcesOfflineWhateverTheExceptionSays() {
        assertEquals(
            ConnectionFailure.OFFLINE,
            classify(SSLHandshakeException("Trust anchor for certification path not found."), false),
        )
        assertEquals(
            ConnectionFailure.OFFLINE,
            classify(UnknownHostException("Unable to resolve host \"plurx-ab12cd.local\""), false),
        )
        assertEquals(ConnectionFailure.OFFLINE, classify(SocketTimeoutException("timeout"), false))
    }

    /**
     * The negative from docs §7. A suite that only checked the happy strings
     * would pass a client that appended "(Unable to resolve host …)" to every
     * one of them.
     */
    @Test
    fun noRenderedCopyContainsTheNativeErrorText() {
        val natives = listOf(
            "Unable to resolve host \"plurx-ab12cd.local\": No address associated with hostname",
            "Trust anchor for certification path not found.",
            "Failed to connect to /192.168.1.10:32400",
            "timeout",
            "unexpected end of stream",
            "HTTP 503 Service Unavailable",
        )
        val rendered = ConnectionFailure.entries.flatMap { failure ->
            listOf(null, "Living Room", "http://plurx-ab12cd.local:32400").flatMap { server ->
                val copy = copyFor(failure, server)
                listOf(copy.title, copy.detail, copy.short)
            }
        }
        for (text in rendered) {
            for (native in natives) {
                assertFalse("'$text' leaks native transport text", text.contains(native))
            }
        }
        // And the old Android sentence in particular, in any casing.
        assertTrue(rendered.none { it.lowercase().contains("unable to resolve host") })
    }

    private fun httpException(code: Int): HttpException = HttpException(
        Response.error<Unit>(code, "{}".toResponseBody("application/json".toMediaType())),
    )
}
