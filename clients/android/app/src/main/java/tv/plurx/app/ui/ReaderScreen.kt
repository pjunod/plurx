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
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.compose.BackHandler
import androidx.annotation.RequiresApi
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
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
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import tv.plurx.app.data.Net
import tv.plurx.app.data.Session
import java.net.URI

internal object NativeReaderHandoff {
    fun shellUrl(origin: String): String? {
        val parsed = runCatching { URI(origin) }.getOrNull() ?: return null
        if (parsed.scheme !in setOf("http", "https") || parsed.host.isNullOrBlank()) return null
        return origin.trimEnd('/') + "/?native-reader=1"
    }

    fun startScript(token: String, itemId: Long, fileId: Long): String? {
        if (token.isEmpty() || itemId < 1 || fileId < 1) return null
        return "window.startNativeReader(${Net.json.encodeToString(token)},$itemId,$fileId);"
    }

    fun permitsNavigation(candidate: String, origin: String): Boolean {
        if (candidate == "about:blank") return true
        val url = runCatching { URI(candidate) }.getOrNull() ?: return false
        val base = runCatching { URI(origin) }.getOrNull() ?: return false
        if (!url.scheme.equals(base.scheme, ignoreCase = true) ||
            !url.host.equals(base.host, ignoreCase = true) || effectivePort(url) != effectivePort(base)
        ) return false
        val path = url.path.orEmpty()
        return path == "/" || path.startsWith("/assets/") || path.startsWith("/api/v1/publication/")
    }

    private fun effectivePort(uri: URI): Int = when {
        uri.port >= 0 -> uri.port
        uri.scheme.equals("https", ignoreCase = true) -> 443
        else -> 80
    }
}

@SuppressLint("SetJavaScriptEnabled")
@Composable
fun ReaderScreen(itemId: Long, fileId: Long, onExit: () -> Unit) {
    var webView by remember { mutableStateOf<WebView?>(null) }
    // Freeze the server identity for this destination. If pairing changes
    // while the old origin is still loading, its WebView must never receive
    // the new server's bearer.
    val origin = remember { Session.origin }
    val shell = remember(origin) { NativeReaderHandoff.shellUrl(origin) }
    var error by remember(shell) {
        mutableStateOf<String?>(shell?.let { null } ?: "The saved Cinema server address is invalid.")
    }

    fun requestClose() {
        val active = webView
        if (active == null) onExit()
        else active.evaluateJavascript(
            "if(typeof closeReader==='function')closeReader();else false;",
        ) { result -> if (result == "false" || result == "null") onExit() }
    }

    BackHandler(onBack = ::requestClose)

    Box(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background)) {
        if (shell != null) {
            AndroidView(
                modifier = Modifier.fillMaxSize(),
                factory = { context ->
                    WebView(context).apply {
                        setBackgroundColor(Color.TRANSPARENT)
                        settings.javaScriptEnabled = true
                        settings.domStorageEnabled = true
                        settings.javaScriptCanOpenWindowsAutomatically = false
                        settings.setSupportMultipleWindows(false)
                        settings.allowFileAccess = false
                        settings.allowContentAccess = false
                        settings.mixedContentMode = android.webkit.WebSettings.MIXED_CONTENT_NEVER_ALLOW
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) settings.safeBrowsingEnabled = true

                        addJavascriptInterface(ReaderBridge { event, message ->
                            when (event) {
                                "close", "session-ended" -> onExit()
                                "ready" -> error = null
                                "error" -> error = message ?: "Cinema could not open this EPUB."
                            }
                        }, "CinemaNative")
                        webViewClient = object : WebViewClient() {
                            private var started = false

                            override fun onPageFinished(view: WebView, url: String) {
                                if (started) return
                                val token = Session.token
                                val script = token?.takeIf { Session.origin == origin }
                                    ?.let { NativeReaderHandoff.startScript(it, itemId, fileId) }
                                if (script == null || !url.startsWith(shell)) {
                                    onExit()
                                    return
                                }
                                started = true
                                view.evaluateJavascript(script, null)
                            }

                            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean =
                                !NativeReaderHandoff.permitsNavigation(request.url.toString(), origin)

                            @Deprecated("Kept for Android 6 WebView")
                            override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean =
                                !NativeReaderHandoff.permitsNavigation(url, origin)

                            override fun onReceivedError(
                                view: WebView,
                                request: WebResourceRequest,
                                resourceError: WebResourceError,
                            ) {
                                if (request.isForMainFrame) error = resourceError.description.toString()
                            }

                            override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                                error = "Android's reader process stopped. Close and reopen this book."
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
                        loadUrl(shell)
                        webView = this
                    }
                },
            )
        }

        error?.let { message ->
            androidx.compose.foundation.layout.Column(
                Modifier.align(Alignment.Center).padding(24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text("Couldn't open this book", style = MaterialTheme.typography.headlineSmall)
                Text(message, modifier = Modifier.padding(vertical = 12.dp))
                Button(onClick = onExit) { Text("Close") }
            }
        }
    }

    DisposableEffect(webView) {
        onDispose {
            webView?.run {
                evaluateJavascript(
                    "if(typeof READER!=='undefined'&&READER&&typeof destroyReader==='function')destroyReader(true);TOKEN=null;ME=null;",
                    null,
                )
                removeJavascriptInterface("CinemaNative")
                stopLoading()
                loadUrl("about:blank")
                clearHistory()
                destroy()
            }
        }
    }
}

private class ReaderBridge(private val onEvent: (String, String?) -> Unit) {
    private val main = Handler(Looper.getMainLooper())

    @JavascriptInterface
    fun postMessage(raw: String) {
        if (raw.length > 1_024) return
        val payload = runCatching { Net.json.parseToJsonElement(raw).jsonObject }.getOrNull() ?: return
        val event = payload["event"]?.jsonPrimitive?.contentOrNull ?: return
        if (event !in setOf("shell-ready", "ready", "close", "session-ended", "error")) return
        val message = payload["message"]?.jsonPrimitive?.contentOrNull
        main.post { onEvent(event, message) }
    }
}
