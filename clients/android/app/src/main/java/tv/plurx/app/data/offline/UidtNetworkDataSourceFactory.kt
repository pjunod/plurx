@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import android.net.Network
import android.net.Uri
import android.os.Build
import androidx.media3.datasource.DataSource
import androidx.media3.datasource.DataSpec
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.datasource.PlaceholderDataSource
import androidx.media3.datasource.TransferListener
import androidx.media3.datasource.okhttp.OkHttpDataSource
import okhttp3.Dns
import okhttp3.OkHttpClient
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean

/**
 * API 34+ transfer sockets must use the network granted to the UIDT job.
 * Until a running job supplies both its socket factory and DNS resolver this
 * factory returns a source that always throws, so Media3 cannot silently fall
 * back to the process default network.
 */
internal class UidtNetworkDataSourceFactory : DataSource.Factory {
    private data class Binding(
        val uriPrefix: String,
        val factory: DataSource.Factory,
        val cancellation: GenerationBoundCancellation,
    )

    private val bindings = ConcurrentHashMap<String, Binding>()

    @Synchronized
    fun bind(downloadId: String, owner: Long, manifestUrl: String, network: Network) {
        val gate = RevocableNetworkGate()
        val client = OkHttpClient.Builder()
            .socketFactory(network.socketFactory)
            .dns(object : Dns {
                override fun lookup(hostname: String) = network.getAllByName(hostname).toList()
            })
            .addInterceptor { chain ->
                if (!gate.isOpen()) throw IOException("UIDT network ownership was revoked")
                chain.proceed(chain.request())
            }
            .build()
        replaceOwnedBinding(
            bindings,
            downloadId,
            Binding(
                manifestUrl.substringBeforeLast('/', missingDelimiterValue = manifestUrl) + "/",
                OkHttpDataSource.Factory(client).setUserAgent(USER_AGENT),
                GenerationBoundCancellation(owner) {
                    gate.revoke()
                    client.dispatcher.cancelAll()
                    client.connectionPool.evictAll()
                },
            ),
        ) { displaced -> displaced.cancellation.revokeCurrent() }
        // The new owner is visible before the old calls are canceled, so an
        // old generation's finally block can never erase the replacement.
    }

    /** Cancels in-flight calls synchronously and only for the owning job generation. */
    @Synchronized
    fun revoke(downloadId: String, owner: Long): Boolean {
        val binding = bindings[downloadId] ?: return false
        if (!binding.cancellation.revoke(owner)) return false
        bindings.remove(downloadId)
        return true
    }

    /** A user policy change intentionally supersedes whichever generation owns the id. */
    @Synchronized
    fun revokeCurrent(downloadId: String) {
        bindings.remove(downloadId)?.cancellation?.revokeCurrent()
    }

    override fun createDataSource(): DataSource {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            return DefaultHttpDataSource.Factory().setUserAgent(USER_AGENT).createDataSource()
        }
        return RoutingDataSource()
    }

    /** Select after DataSpec is known, so concurrent jobs keep their own Network. */
    private inner class RoutingDataSource : DataSource {
        private val listeners = mutableListOf<TransferListener>()
        private var delegate: DataSource? = null

        override fun addTransferListener(transferListener: TransferListener) {
            listeners += transferListener
            delegate?.addTransferListener(transferListener)
        }

        override fun open(dataSpec: DataSpec): Long {
            val factory = bindings.values
                .filter { dataSpec.uri.toString().startsWith(it.uriPrefix) }
                .maxByOrNull { it.uriPrefix.length }
                ?.factory
                ?: PlaceholderDataSource.FACTORY
            return factory.createDataSource().also { source ->
                listeners.forEach(source::addTransferListener)
                delegate = source
            }.open(dataSpec)
        }

        override fun read(buffer: ByteArray, offset: Int, length: Int): Int =
            checkNotNull(delegate) { "offline data source was not opened" }
                .read(buffer, offset, length)

        override fun getUri(): Uri? = delegate?.uri

        override fun close() {
            delegate?.close()
            delegate = null
        }
    }

    private companion object {
        const val USER_AGENT = "Cinema Android offline"
    }
}

internal class RevocableNetworkGate {
    private val open = AtomicBoolean(true)

    fun isOpen(): Boolean = open.get()

    fun revoke() {
        open.set(false)
    }
}

internal fun <T> replaceOwnedBinding(
    bindings: MutableMap<String, T>,
    id: String,
    replacement: T,
    revokeDisplaced: (T) -> Unit,
) {
    bindings.put(id, replacement)?.let(revokeDisplaced)
}

/** Pure generation guard used by the immediate OkHttp cancellation path. */
internal class GenerationBoundCancellation(
    private val owner: Long,
    private val cancelNow: () -> Unit,
) {
    private var revoked = false

    @Synchronized
    fun revoke(requester: Long): Boolean {
        if (requester != owner || revoked) return false
        revoked = true
        cancelNow()
        return true
    }

    @Synchronized
    fun revokeCurrent(): Boolean {
        if (revoked) return false
        revoked = true
        cancelNow()
        return true
    }
}
