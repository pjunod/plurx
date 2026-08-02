package tv.plurx.app.data

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import java.io.IOException
import java.net.Inet4Address
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull

private const val PlurxServiceType = "_plurx._tcp."

/** A LAN mDNS resolution that takes longer than this is not coming back. */
private const val RESOLVE_TIMEOUT_MS = 5_000L

class DiscoveredServer internal constructor(
    val id: String,
    val name: String,
    val detail: String,
    internal val serviceKey: String,
    internal val serviceInfo: NsdServiceInfo,
)

data class ServerDiscoveryState(
    val servers: List<DiscoveredServer> = emptyList(),
    val isSearching: Boolean = false,
    val error: String? = null,
)

/**
 * Finds the `_plurx._tcp` service published by plurxd. Discovery is scoped to
 * the connect screen; a selected result is resolved only when the user picks
 * it, keeping multicast work and stale addresses to a minimum.
 */
class ServerDiscovery(context: Context) {
    private val manager = context.applicationContext
        .getSystemService(Context.NSD_SERVICE) as NsdManager
    private val lock = Any()
    private val found = linkedMapOf<String, DiscoveredServer>()
    private var listener: NsdManager.DiscoveryListener? = null
    private var nextResultId = 0L

    private val _state = MutableStateFlow(ServerDiscoveryState())
    val state: StateFlow<ServerDiscoveryState> = _state.asStateFlow()

    fun start() {
        synchronized(lock) {
            if (listener != null) return
        }

        val discoveryListener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) {
                updateState(isSearching = true, error = null)
            }

            override fun onServiceFound(serviceInfo: NsdServiceInfo) {
                if (!isPlurxService(serviceInfo.serviceType)) return
                synchronized(lock) {
                    nextResultId += 1
                    DiscoveredServer(
                        id = "${serviceKey(serviceInfo)}|$nextResultId",
                        name = serviceInfo.serviceName.ifBlank { "plurx" },
                        detail = serviceDetail(serviceInfo),
                        serviceKey = serviceKey(serviceInfo),
                        serviceInfo = serviceInfo,
                    ).also { found[it.id] = it }
                }
                updateState()
            }

            override fun onServiceLost(serviceInfo: NsdServiceInfo) {
                val key = serviceKey(serviceInfo)
                synchronized(lock) {
                    val exact = found.entries.firstOrNull {
                        it.value.serviceKey == key &&
                            it.value.serviceInfo.toString() == serviceInfo.toString()
                    }
                    val match = exact ?: found.entries.firstOrNull { it.value.serviceKey == key }
                    if (match != null) found.remove(match.key)
                }
                updateState()
            }

            override fun onDiscoveryStopped(serviceType: String) {
                updateState(isSearching = false)
            }

            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                synchronized(lock) { listener = null }
                updateState(
                    isSearching = false,
                    error = "Couldn't search this network. You can still add a server manually.",
                )
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
                synchronized(lock) { listener = null }
                updateState(isSearching = false)
            }
        }

        synchronized(lock) { listener = discoveryListener }
        updateState(isSearching = true, error = null)
        try {
            manager.discoverServices(
                PlurxServiceType,
                NsdManager.PROTOCOL_DNS_SD,
                discoveryListener,
            )
        } catch (_: SecurityException) {
            synchronized(lock) { listener = null }
            updateState(
                isSearching = false,
                error = "Allow local-network access to find plurx servers automatically.",
            )
        } catch (_: RuntimeException) {
            synchronized(lock) { listener = null }
            updateState(
                isSearching = false,
                error = "Couldn't search this network. You can still add a server manually.",
            )
        }
    }

    fun stop() {
        val current = synchronized(lock) {
            val value = listener
            listener = null
            value
        } ?: return
        try {
            manager.stopServiceDiscovery(current)
        } catch (_: RuntimeException) {
            // The framework can report an already-stopped listener after a
            // network handoff. The screen is leaving either way.
        }
        updateState(isSearching = false)
    }

    fun clear() {
        synchronized(lock) { found.clear() }
        updateState(error = null)
    }

    /**
     * Resolve a discovered service to an origin.
     *
     * Bounded, because `NsdManager.resolveService` is a callback with no
     * timeout of its own: a service that vanished between discovery and
     * selection — a server that just went to sleep, a stale multicast record —
     * never calls either callback, and the suspension never ends. The caller
     * sets `busy = true` before this and clears it in a `finally`, so an
     * unbounded wait wedges the connect screen with a spinner and no way out.
     */
    @Suppress("DEPRECATION")
    suspend fun resolve(server: DiscoveredServer): String =
        withTimeoutOrNull(RESOLVE_TIMEOUT_MS) { awaitResolve(server) }
            ?: throw IOException("The discovered server stopped responding.")

    @Suppress("DEPRECATION")
    private suspend fun awaitResolve(server: DiscoveredServer): String = suspendCancellableCoroutine { continuation ->
        manager.resolveService(server.serviceInfo, object : NsdManager.ResolveListener {
            override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                if (continuation.isActive) {
                    continuation.resumeWith(
                        Result.failure(IOException("The discovered server stopped responding.")),
                    )
                }
            }

            override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                val addresses = if (Build.VERSION.SDK_INT >= 34) {
                    serviceInfo.hostAddresses
                } else {
                    listOfNotNull(serviceInfo.host)
                }
                val address = addresses.firstOrNull { it is Inet4Address }
                    ?: addresses.firstOrNull()
                val host = address?.hostAddress
                val result = if (host != null && serviceInfo.port > 0) {
                    runCatching { nsdOrigin(host, serviceInfo.port) }
                } else {
                    Result.failure(IOException("The discovered server did not publish a usable address."))
                }
                if (continuation.isActive) continuation.resumeWith(result)
            }
        })
    }

    /** Wait briefly for NSD after launch or a network handoff. */
    suspend fun availableServers(timeoutMs: Long = 3_000): List<DiscoveredServer> {
        start()
        var waitedMs = 0L
        while (_state.value.servers.isEmpty() && waitedMs < timeoutMs) {
            delay(150)
            waitedMs += 150
        }
        return _state.value.servers
    }

    private fun updateState(
        isSearching: Boolean = _state.value.isSearching,
        error: String? = _state.value.error,
    ) {
        val servers = synchronized(lock) {
            found.values.sortedWith(compareBy(String.CASE_INSENSITIVE_ORDER) { it.name })
        }
        _state.value = ServerDiscoveryState(servers, isSearching, error)
    }

    private fun isPlurxService(type: String): Boolean =
        type.substringBefore(',').trimEnd('.').equals("_plurx._tcp", ignoreCase = true)

    private fun serviceKey(service: NsdServiceInfo): String =
        "${service.serviceName}|${service.serviceType.trimEnd('.')}"

    private fun serviceDetail(service: NsdServiceInfo): String {
        val address = if (Build.VERSION.SDK_INT >= 34) {
            service.hostAddresses.firstOrNull { it is Inet4Address }
                ?: service.hostAddresses.firstOrNull()
        } else {
            null
        }
        if (address != null) return address.hostAddress ?: "plurx server"

        val instanceId = service.attributes["id"]
            ?.toString(Charsets.UTF_8)
            ?.trim()
            ?.takeIf { it.isNotEmpty() }
        return instanceId?.let { "Server …${it.takeLast(6)}" } ?: "plurx server"
    }
}

internal fun nsdOrigin(rawHost: String, port: Int): String {
    require(port in 1..65535) { "Invalid server port" }
    val host = rawHost.trim().trimEnd('.')
    require(host.isNotEmpty()) { "Missing server host" }
    val formatted = if (':' in host && !host.startsWith('[')) {
        "[${host.replace("%", "%25")}]"
    } else {
        host
    }
    return "http://$formatted:$port"
}
