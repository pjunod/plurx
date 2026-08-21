package tv.plurx.app.ui

import android.annotation.SuppressLint
import android.graphics.Color
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.webkit.JavascriptInterface
import android.webkit.RenderProcessGoneDetail
import android.webkit.SafeBrowsingResponse
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.compose.BackHandler
import androidx.annotation.RequiresApi
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.launch
import kotlinx.serialization.Serializable
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import tv.plurx.app.data.Net
import tv.plurx.app.data.PublicationLimits
import tv.plurx.app.data.PublicationManifest
import tv.plurx.app.data.ReadingLocator
import tv.plurx.app.data.offline.OfflineBook
import tv.plurx.app.data.offline.OfflineBookPreferences
import tv.plurx.app.data.offline.OfflineBooks
import tv.plurx.app.data.offline.safePublicationPath
import java.io.ByteArrayInputStream
import java.io.File
import java.io.FileInputStream
import java.io.FilterInputStream
import java.io.InputStream
import java.net.URI
import java.util.concurrent.Semaphore
import java.util.concurrent.atomic.AtomicBoolean

/** A synthetic, app-owned HTTPS origin. Every request is intercepted below;
 * WebView is never allowed to resolve this host or publisher-controlled URLs. */
internal object OfflineBookWebPolicy {
    const val ORIGIN = "https://offline.cinema.invalid"
    const val SHELL = "$ORIGIN/offline-reader.html"
    const val PUBLICATION_PREFIX = "/publication/"

    fun permitsNavigation(candidate: String): Boolean {
        if (candidate == "about:blank") return true
        val uri = runCatching { URI(candidate) }.getOrNull() ?: return false
        if (!uri.scheme.equals("https", ignoreCase = true) ||
            !uri.host.equals("offline.cinema.invalid", ignoreCase = true) ||
            effectivePort(uri) != 443 || uri.rawQuery != null
        ) return false
        val path = uri.rawPath.orEmpty()
        if (uri.normalize().rawPath != path) return false
        return path in setOf("/offline-reader.html", "/reader.js", "/offline-reader.js") ||
            path.startsWith(PUBLICATION_PREFIX)
    }

    fun publicationPath(candidate: String): String? {
        val uri = runCatching { URI(candidate) }.getOrNull() ?: return null
        if (!permitsNavigation(candidate) || !uri.rawPath.startsWith(PUBLICATION_PREFIX)) return null
        return safePublicationPath(uri.rawPath.removePrefix(PUBLICATION_PREFIX))
    }

    private fun effectivePort(uri: URI): Int = if (uri.port >= 0) uri.port else 443
}

@Serializable
private data class OfflineReaderPayload(
    val publication: PublicationManifest,
    val limits: PublicationLimits,
    val resourceBase: String = "${OfflineBookWebPolicy.ORIGIN}${OfflineBookWebPolicy.PUBLICATION_PREFIX}",
    val locator: ReadingLocator? = null,
    val progression: Double = 0.0,
    val completed: Boolean = false,
    val preferences: OfflineBookPreferences = OfflineBookPreferences(),
)

@Serializable
private data class OfflineReaderEvent(
    val event: String,
    val message: String? = null,
    val locator: ReadingLocator? = null,
    val progression: Double? = null,
    val completed: Boolean? = null,
    val recorded_at: Long? = null,
    val preferences: OfflineBookPreferences? = null,
)

@SuppressLint("SetJavaScriptEnabled")
@Composable
fun OfflineBookReaderScreen(bookId: String, onExit: () -> Unit) {
    val records by OfflineBooks.records.collectAsStateWithLifecycle()
    val book = records.firstOrNull { it.id == bookId }
    val scope = rememberCoroutineScope()
    var webView by remember(bookId) { mutableStateOf<WebView?>(null) }
    var error by remember(bookId) { mutableStateOf<String?>(null) }

    fun requestClose() {
        val active = webView
        if (active == null) onExit()
        else active.evaluateJavascript(
            "window.dispatchEvent(new Event('pagehide'));true;",
        ) { onExit() }
    }

    BackHandler(onBack = ::requestClose)

    Box(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background)) {
        if (book?.isPlayable == true) {
            AndroidView(
                modifier = Modifier.fillMaxSize(),
                factory = { context ->
                    val resolver = OfflineBookResourceResolver(context.assets::open, book)
                    WebView(context).apply {
                        setBackgroundColor(Color.TRANSPARENT)
                        settings.javaScriptEnabled = true
                        settings.domStorageEnabled = false
                        settings.javaScriptCanOpenWindowsAutomatically = false
                        settings.setSupportMultipleWindows(false)
                        settings.allowFileAccess = false
                        settings.allowContentAccess = false
                        settings.cacheMode = android.webkit.WebSettings.LOAD_NO_CACHE
                        settings.mixedContentMode = android.webkit.WebSettings.MIXED_CONTENT_NEVER_ALLOW
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) settings.safeBrowsingEnabled = true

                        addJavascriptInterface(OfflineReaderBridge { event ->
                            when (event.event) {
                                "ready" -> error = null
                                "close" -> onExit()
                                "error" -> error = event.message ?: "Cinema could not open this EPUB."
                                "progress" -> {
                                    val locator = event.locator
                                    val progression = event.progression
                                    val completed = event.completed
                                    val recordedAt = event.recorded_at
                                    val preferences = event.preferences
                                    if (locator != null && progression != null && completed != null &&
                                        recordedAt != null && preferences != null
                                    ) {
                                        scope.launch {
                                            OfflineBooks.record(
                                                id = book.id,
                                                locator = locator,
                                                progression = progression,
                                                completed = completed,
                                                recordedAt = recordedAt,
                                                preferences = preferences,
                                            )
                                        }
                                    }
                                }
                            }
                        }, "CinemaOffline")
                        webViewClient = object : WebViewClient() {
                            private var started = false

                            override fun shouldInterceptRequest(
                                view: WebView,
                                request: WebResourceRequest,
                            ): WebResourceResponse = resolver.resolve(request.url.toString())

                            override fun shouldOverrideUrlLoading(
                                view: WebView,
                                request: WebResourceRequest,
                            ): Boolean = !OfflineBookWebPolicy.permitsNavigation(request.url.toString())

                            @Deprecated("Kept for Android 6 WebView")
                            override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean =
                                !OfflineBookWebPolicy.permitsNavigation(url)

                            override fun onPageFinished(view: WebView, url: String) {
                                if (started || url != OfflineBookWebPolicy.SHELL) return
                                val publication = book.publication
                                val limits = book.limits
                                if (publication == null || limits == null) {
                                    error = "This local EPUB is incomplete. Download it again."
                                    return
                                }
                                started = true
                                val payload = Net.json.encodeToString(
                                    OfflineReaderPayload(
                                        publication = publication,
                                        limits = limits,
                                        locator = book.locator,
                                        progression = book.progression,
                                        completed = book.completed,
                                        preferences = book.preferences,
                                    ),
                                )
                                view.evaluateJavascript("window.startOfflineReader($payload);") { result ->
                                    if (result == "false" || result == "null") {
                                        error = "Cinema could not start the local EPUB reader."
                                    }
                                }
                            }

                            override fun onReceivedError(
                                view: WebView,
                                request: WebResourceRequest,
                                resourceError: WebResourceError,
                            ) {
                                if (request.isForMainFrame) error = resourceError.description.toString()
                            }

                            override fun onRenderProcessGone(
                                view: WebView,
                                detail: RenderProcessGoneDetail,
                            ): Boolean {
                                error = "Android's reader process stopped. Close and reopen this book."
                                view.destroy()
                                return true
                            }

                            @RequiresApi(Build.VERSION_CODES.O_MR1)
                            override fun onSafeBrowsingHit(
                                view: WebView,
                                request: WebResourceRequest,
                                threatType: Int,
                                callback: SafeBrowsingResponse,
                            ) {
                                callback.backToSafety(true)
                                error = "Android blocked an unsafe reader navigation."
                            }
                        }
                        loadUrl(OfflineBookWebPolicy.SHELL)
                        webView = this
                    }
                },
            )
        }

        val visibleError = error ?: when {
            book == null -> "This download is no longer on the device."
            !book.isPlayable -> "This local EPUB is incomplete. Download it again."
            else -> null
        }
        visibleError?.let { message ->
            Column(
                Modifier.align(Alignment.Center).padding(24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text("Couldn't open this download", style = MaterialTheme.typography.headlineSmall)
                Text(message, modifier = Modifier.padding(vertical = 12.dp))
                Button(onClick = onExit) { Text("Close") }
            }
        }
    }

    DisposableEffect(webView) {
        onDispose {
            webView?.run {
                evaluateJavascript("window.dispatchEvent(new Event('pagehide'));void 0", null)
                removeJavascriptInterface("CinemaOffline")
                stopLoading()
                loadUrl("about:blank")
                clearHistory()
                clearCache(true)
                destroy()
            }
        }
    }
}

/** Resolves the reader shell and publisher resources from app-private files.
 * Any request outside the synthetic origin gets a local 403 response, so CSS
 * imports, images, frames, and fetches cannot escape through WebView. */
private class OfflineBookResourceResolver(
    private val openAsset: (String) -> InputStream,
    private val book: OfflineBook,
) {
    private val publicationRoot = File(OfflineBooks.root(book), "publication")
    private val resourcePermits = Semaphore(
        (book.limits?.concurrent_resource_reads ?: 1).coerceIn(1, 2),
        true,
    )

    fun resolve(candidate: String): WebResourceResponse {
        val uri = runCatching { URI(candidate) }.getOrNull() ?: return denied(400, "Bad Request")
        if (!OfflineBookWebPolicy.permitsNavigation(candidate)) return denied(403, "Forbidden")
        return when (uri.rawPath) {
            "/offline-reader.html" -> asset("offline-reader.html", "text/html")
            "/reader.js" -> asset("reader.js", "text/javascript")
            "/offline-reader.js" -> asset("offline-reader.js", "text/javascript")
            else -> publication(candidate)
        }
    }

    private fun asset(name: String, mime: String): WebResourceResponse = runCatching {
        response(200, "OK", mime, openAsset(name))
    }.getOrElse { denied(404, "Not Found") }

    private fun publication(candidate: String): WebResourceResponse {
        val path = OfflineBookWebPolicy.publicationPath(candidate) ?: return denied(404, "Not Found")
        val file = OfflineBooks.catalog.safeChild(publicationRoot, path)
            ?.takeIf(File::isFile) ?: return denied(404, "Not Found")
        val maximum = book.limits?.resource_bytes ?: return denied(409, "Incomplete")
        if (file.length() > maximum) return denied(413, "Content Too Large")
        return try {
            resourcePermits.acquire()
            response(
                200,
                "OK",
                OfflineBooks.mimeType(book, path),
                PermitInputStream(FileInputStream(file), resourcePermits),
                file.length(),
            )
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
            denied(503, "Unavailable")
        } catch (_: Exception) {
            resourcePermits.release()
            denied(404, "Not Found")
        }
    }

    private fun response(
        status: Int,
        reason: String,
        mime: String,
        body: InputStream,
        length: Long? = null,
    ): WebResourceResponse = WebResourceResponse(
        mime,
        if (isText(mime)) "utf-8" else null,
        status,
        reason,
        buildMap {
            put("Cache-Control", "no-store")
            put("X-Content-Type-Options", "nosniff")
            if (length != null) put("Content-Length", length.toString())
        },
        body,
    )

    private fun denied(status: Int, reason: String): WebResourceResponse = response(
        status,
        reason,
        "text/plain",
        ByteArrayInputStream(reason.toByteArray()),
        reason.length.toLong(),
    )

    private fun isText(mime: String): Boolean =
        mime.startsWith("text/") || mime.contains("xml") || mime.contains("javascript") ||
            mime.contains("json") || mime.contains("svg")
}

private class PermitInputStream(
    input: InputStream,
    private val semaphore: Semaphore,
) : FilterInputStream(input) {
    private val released = AtomicBoolean(false)

    override fun read(): Int = super.read().also { if (it < 0) release() }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int =
        super.read(buffer, offset, length).also { if (it < 0) release() }

    override fun close() {
        try {
            super.close()
        } finally {
            release()
        }
    }

    private fun release() {
        if (released.compareAndSet(false, true)) semaphore.release()
    }
}

private class OfflineReaderBridge(private val onEvent: (OfflineReaderEvent) -> Unit) {
    private val main = Handler(Looper.getMainLooper())

    @JavascriptInterface
    fun postMessage(raw: String) {
        if (raw.length > 64 * 1_024) return
        val event = runCatching { Net.json.decodeFromString<OfflineReaderEvent>(raw) }.getOrNull() ?: return
        if (event.event !in setOf("shell-ready", "ready", "progress", "close", "error", "status")) return
        main.post { onEvent(event) }
    }
}
