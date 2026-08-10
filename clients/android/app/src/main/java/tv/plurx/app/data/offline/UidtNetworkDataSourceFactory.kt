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
import java.util.concurrent.ConcurrentHashMap

/**
 * API 34+ transfer sockets must use the network granted to the UIDT job.
 * Until a running job supplies both its socket factory and DNS resolver this
 * factory returns a source that always throws, so Media3 cannot silently fall
 * back to the process default network.
 */
internal class UidtNetworkDataSourceFactory : DataSource.Factory {
    private data class Binding(val uriPrefix: String, val factory: DataSource.Factory)

    private val bindings = ConcurrentHashMap<String, Binding>()

    fun bind(downloadId: String, manifestUrl: String, network: Network) {
        val client = OkHttpClient.Builder()
            .socketFactory(network.socketFactory)
            .dns(object : Dns {
                override fun lookup(hostname: String) = network.getAllByName(hostname).toList()
            })
            .build()
        bindings[downloadId] = Binding(
            manifestUrl.substringBeforeLast('/', missingDelimiterValue = manifestUrl) + "/",
            OkHttpDataSource.Factory(client).setUserAgent(USER_AGENT),
        )
    }

    fun clear(downloadId: String) {
        bindings.remove(downloadId)
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
