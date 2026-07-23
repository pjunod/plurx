package tv.plurx.app.data

/**
 * Live connection state, read by the OkHttp auth interceptor (and the image /
 * player data sources) on every request. Set once at connect and after login;
 * kept as a plain holder so a single OkHttpClient serves the whole app and the
 * token can change without rebuilding it.
 */
object Session {
    /** Server origin, no trailing slash, e.g. `http://192.168.1.10:32600`. */
    @Volatile
    var origin: String = ""

    /** Bearer token, or null when signed out. */
    @Volatile
    var token: String? = null

    /** Absolute URL for a server-relative path (`/api/v1/images/…`). */
    fun url(path: String): String =
        if (path.startsWith("http")) path else origin + path
}
