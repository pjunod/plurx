package tv.plurx.app.data

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import kotlinx.coroutines.CancellationException
import kotlinx.serialization.SerializationException
import retrofit2.HttpException
import java.io.IOException
import java.io.InterruptedIOException
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.PortUnreachableException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.security.cert.CertificateException
import javax.net.ssl.SSLException

/**
 * The one place on Android that turns a transport failure into words.
 *
 * A native error string is diagnostics, never user copy: this file exists so
 * that `Unable to resolve host "plurx-ab12cd.local": No address associated with
 * hostname` never reaches a viewer again. Nothing outside this file may
 * construct connectivity copy — see `docs/CLIENT-CONNECTIVITY.md` §2.2.
 *
 * **`tests/contracts/connectivity-copy.json` is the source of truth for every
 * string below.** The table here is a transcription of it, and
 * `ConnectivityCopyTest` reads that JSON off the test classpath and asserts the
 * two are byte-identical — including `{server}` interpolation, the action
 * lists, and the negative (no copy string contains the native exception text).
 * Edit the JSON first; the test will tell you what to change here.
 */
enum class ConnectionFailure(val id: String) {
    OFFLINE("offline"),
    UNREACHABLE("unreachable"),
    UNKNOWN_HOST("unknown_host"),
    TIMEOUT("timeout"),
    INSECURE("insecure"),
    SERVER_ERROR("server_error"),
    UNKNOWN("unknown"),
}

/** The actions a class offers. `retry` is on every class, by contract rule. */
enum class ConnectionAction(val id: String, val label: String) {
    Retry("retry", "Try again"),
    ChangeServer("change_server", "Change server"),
}

data class ConnectionCopy(
    val title: String,
    val detail: String,
    val short: String,
    val actions: List<ConnectionAction>,
)

/** The contract's `server_fallback`, used when there is no name and no origin. */
const val SERVER_FALLBACK = "the server"

/** Which of docs §5's three shapes a surface should draw. */
enum class ConnectionSurface { None, Banner, Full }

/**
 * `cached_content_wins`, as one decision every screen shares.
 *
 * A failed refresh over content the viewer is already reading gets a one-line
 * banner and keeps the content; a failure with nothing behind it gets the full
 * surface. Four screens deciding this independently is how three of them ended
 * up either blanking the content or showing nothing at all.
 */
fun connectionSurfaceFor(failure: ConnectionFailure?, hasContent: Boolean): ConnectionSurface = when {
    failure == null -> ConnectionSurface.None
    hasContent -> ConnectionSurface.Banner
    else -> ConnectionSurface.Full
}

/**
 * Classify a failure — a **pure** function of the throwable and one boolean.
 *
 * [hasNetwork] is passed in rather than read from a `Context` so this is unit
 * testable without the Android framework; [hasValidatedNetwork] is the
 * production source of that boolean and deliberately lives apart from here.
 *
 * Returns `null` when the throwable is **not a connectivity failure at all**,
 * which is two distinct things and callers usually have to tell them apart:
 *
 * - **HTTP 401/403.** An expired token has always had its own path (sign out,
 *   re-authenticate); folding it in here would make it look like a network
 *   problem. Callers keep their existing auth handling.
 * - **Cancellation.** A screen leaving composition or a superseded request is
 *   the caller saying stop, not a failure to report. [isCancellation] is the
 *   predicate; the guard lives here rather than in every `catch` so that no
 *   caller can forget it and paint "Something went wrong" over a navigation.
 *
 * Order follows docs §2.2 exactly. `SSLException`, `UnknownHostException`,
 * `SocketTimeoutException` and `ConnectException` are all `IOException`s, so
 * the general `IOException` case must come last or it swallows all of them.
 */
fun classify(error: Throwable, hasNetwork: Boolean): ConnectionFailure? {
    val named = classifyNamedTransport(error, hasNetwork)
    if (named != null) return named
    if (isCancellation(error) || isAuthResponse(error)) return null
    // The general `IOException` case from docs §2.2, last of all. "It may be
    // powered off, restarting, or on another network" stays honest for the
    // IOExceptions the table does not name.
    if (causeChain(error).any { it is IOException }) return ConnectionFailure.UNREACHABLE
    return ConnectionFailure.UNKNOWN
}

/**
 * [classify] without its two catch-all rows: a class only when the chain
 * actually names a transport failure the taxonomy recognises, `null`
 * otherwise.
 *
 * This exists for surfaces whose failures are *not* all transport failures.
 * Media3's offline transfer is the one: a cache write that ran out of disk and
 * a response the server genuinely sent are both `IOException`s, so
 * [classify]'s last row would answer "Can't reach {server}" for a full device
 * and send the viewer to power-cycle a router that was working.
 */
fun classifyNamedTransport(error: Throwable, hasNetwork: Boolean): ConnectionFailure? {
    // Retrofit, OkHttp and the coroutine machinery all wrap: the interesting
    // exception is frequently a cause two links down.
    val chain = causeChain(error)

    if (chain.any { it is CancellationException }) return null

    // Checked before the offline shortcut: a 401 body can only have arrived
    // over a working network, so a capability read that has since gone false
    // must not turn an expired token into "You're offline".
    if (chain.any { it is HttpException && (it.code() == 401 || it.code() == 403) }) return null

    // Every exception below can occur with the radio off, so the system's
    // answer outranks the exception's. This is what turns "Unable to resolve
    // host" into "You're offline" on a plane.
    if (!hasNetwork) return ConnectionFailure.OFFLINE

    // Specific first, across the whole chain: an `IOException` wrapping an
    // `SSLHandshakeException` is a certificate problem, and answering
    // "unreachable" because the outermost link happened to be generic would
    // send the viewer looking at the wrong thing.
    for (link in chain) {
        specificClass(link)?.let { return it }
    }
    return null
}

/**
 * The caller said stop. Not a failure, and nothing may be rendered for it.
 *
 * `catch (e: Exception)` includes `CancellationException` in Kotlin, so every
 * `catch` in the app is a place a cancelled screen could have painted an error
 * over itself. One predicate, checked inside [classify].
 */
fun isCancellation(error: Throwable): Boolean =
    causeChain(error).any { it is CancellationException }

/** HTTP 401/403 — the auth path, not a connectivity class. */
fun isAuthResponse(error: Throwable): Boolean = causeChain(error).any {
    it is HttpException && (it.code() == 401 || it.code() == 403)
}

private fun specificClass(error: Throwable): ConnectionFailure? = when (error) {
    // SSLHandshakeException and SSLPeerUnverifiedException are SSLExceptions.
    is SSLException, is CertificateException -> ConnectionFailure.INSECURE
    is UnknownHostException -> ConnectionFailure.UNKNOWN_HOST
    // SocketTimeoutException is an InterruptedIOException; so is the
    // `callTimeout` OkHttp throws when the API deadline in `Net` expires.
    is SocketTimeoutException, is InterruptedIOException -> ConnectionFailure.TIMEOUT
    is ConnectException, is NoRouteToHostException, is PortUnreachableException ->
        ConnectionFailure.UNREACHABLE
    // Reached and answered, just not usefully.
    is HttpException -> if (error.code() in 500..599) ConnectionFailure.SERVER_ERROR else null
    is SerializationException -> ConnectionFailure.SERVER_ERROR
    else -> null
}

/** The throwable and its causes, innermost last, cycle-safe. */
private fun causeChain(error: Throwable): List<Throwable> {
    val chain = mutableListOf<Throwable>()
    var current: Throwable? = error
    while (current != null && chain.size < 16 && chain.none { it === current }) {
        chain += current
        current = current.cause
    }
    return chain
}

/**
 * The copy for a class, with `{server}` resolved.
 *
 * [server] is the server's display name, falling back to its origin, falling
 * back to [SERVER_FALLBACK] — callers pass whichever they have.
 */
fun copyFor(failure: ConnectionFailure, server: String?): ConnectionCopy {
    val name = server?.trim()?.takeIf { it.isNotEmpty() } ?: SERVER_FALLBACK
    val template = COPY.getValue(failure)
    return template.copy(
        title = template.title.replace(SERVER_TOKEN, name),
        detail = template.detail.replace(SERVER_TOKEN, name),
        short = template.short.replace(SERVER_TOKEN, name),
    )
}

private const val SERVER_TOKEN = "{server}"

// Transcribed verbatim from tests/contracts/connectivity-copy.json (version 1).
private val COPY: Map<ConnectionFailure, ConnectionCopy> = mapOf(
    ConnectionFailure.OFFLINE to ConnectionCopy(
        title = "You're offline",
        detail = "This device isn't connected to a network.",
        short = "You're offline.",
        actions = listOf(ConnectionAction.Retry),
    ),
    ConnectionFailure.UNREACHABLE to ConnectionCopy(
        title = "Can't reach {server}",
        detail = "The network is working, but the server didn't answer. It may be powered off, restarting, or on another network.",
        short = "Can't reach {server}.",
        actions = listOf(ConnectionAction.Retry, ConnectionAction.ChangeServer),
    ),
    ConnectionFailure.UNKNOWN_HOST to ConnectionCopy(
        title = "Can't find {server}",
        detail = "Nothing on this network answers to that address. If the server moved, point Cinema at its new one.",
        short = "Can't find {server}.",
        actions = listOf(ConnectionAction.Retry, ConnectionAction.ChangeServer),
    ),
    ConnectionFailure.TIMEOUT to ConnectionCopy(
        title = "No answer from {server}",
        detail = "The server accepted the connection but didn't answer in time. It may be busy or still starting up.",
        short = "No answer from {server}.",
        actions = listOf(ConnectionAction.Retry),
    ),
    ConnectionFailure.INSECURE to ConnectionCopy(
        title = "Couldn't connect securely to {server}",
        detail = "The secure connection failed. The server's certificate may have changed or expired.",
        short = "Couldn't connect securely to {server}.",
        actions = listOf(ConnectionAction.Retry, ConnectionAction.ChangeServer),
    ),
    ConnectionFailure.SERVER_ERROR to ConnectionCopy(
        title = "Error from {server}",
        detail = "The server answered with an error. Nothing is wrong with this device or your network.",
        short = "Error from {server}.",
        actions = listOf(ConnectionAction.Retry),
    ),
    ConnectionFailure.UNKNOWN to ConnectionCopy(
        title = "Something went wrong",
        detail = "Cinema couldn't complete that request.",
        short = "Something went wrong.",
        actions = listOf(ConnectionAction.Retry),
    ),
)

/**
 * Does this device have a network the system considers usable?
 *
 * Kept apart from [classify] on purpose: this needs a `Context` and the
 * classifier must stay pure. `OfflineDownloads.uidtNetworkSatisfiesCurrentPolicy`
 * reads capabilities the same way for the transfer policy.
 *
 * The framework read and the decision are split so the decision can be unit
 * tested: [networkIsUsable] is pure booleans, and what remains here is four
 * lines of `ConnectivityManager` that only an instrumented test can reach.
 * `ConnectivityReadTest` (androidTest) covers those.
 */
fun hasValidatedNetwork(context: Context): Boolean {
    // A device with no ConnectivityManager at all is not evidence of being
    // offline, so it is not reported as such.
    val manager = context.getSystemService(ConnectivityManager::class.java) ?: return true
    val capabilities = manager.activeNetwork?.let(manager::getNetworkCapabilities)
    return networkIsUsable(
        hasCapabilities = capabilities != null,
        hasInternet = capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true,
        isValidated = capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true,
    )
}

/**
 * The `offline` decision, as three booleans.
 *
 * `NET_CAPABILITY_VALIDATED` is the one that matters: an interface that is up
 * but has not passed the system's connectivity probe is what a viewer means by
 * "the wifi isn't working", and it is the difference between "You're offline"
 * and "Unable to resolve host" on a plane.
 */
internal fun networkIsUsable(
    hasCapabilities: Boolean,
    hasInternet: Boolean,
    isValidated: Boolean,
): Boolean = hasCapabilities && hasInternet && isValidated
