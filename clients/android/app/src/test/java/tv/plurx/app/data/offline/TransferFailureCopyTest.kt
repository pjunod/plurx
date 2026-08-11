@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import androidx.media3.datasource.cache.Cache
import kotlinx.coroutines.CancellationException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test
import tv.plurx.app.data.ConnectionFailure
import tv.plurx.app.data.copyFor
import java.io.IOException
import java.net.ConnectException
import java.net.UnknownHostException
import javax.net.ssl.SSLHandshakeException

/**
 * A download fails for reasons that are not the network, and Media3 reports
 * every one of them as an `IOException`. Handing them all to the connectivity
 * classifier told a viewer whose 12 GB download had filled their device to go
 * and check their router.
 */
class TransferFailureCopyTest {

    @Test
    fun aFullDeviceIsNotANetworkProblem() {
        // Media3 writes into the app's own cache directory; a commit failure
        // there is a disk problem wearing an IOException.
        assertEquals(
            NOT_ENOUGH_STORAGE,
            transferFailureMessage(Cache.CacheException("Failed to commit content"), true),
        )
        assertEquals(
            NOT_ENOUGH_STORAGE,
            transferFailureMessage(IOException("write failed: ENOSPC (No space left on device)"), true),
        )
        assertEquals(
            NOT_ENOUGH_STORAGE,
            transferFailureMessage(
                IOException("transfer failed", IOException("No space left on device")),
                true,
            ),
        )
        // The regression this test exists for, stated plainly.
        val disk = transferFailureMessage(Cache.CacheException("no space"), true)
        assertFalse(disk == copyFor(ConnectionFailure.UNREACHABLE, null).short)
        assertFalse(disk.orEmpty().contains("Can't reach"))
    }

    @Test
    fun aGenuineTransportFailureStillGetsItsContractClass() {
        assertEquals(
            copyFor(ConnectionFailure.UNKNOWN_HOST, null).short,
            transferFailureMessage(UnknownHostException("plurx-ab12cd.local"), true),
        )
        assertEquals(
            copyFor(ConnectionFailure.UNREACHABLE, null).short,
            transferFailureMessage(ConnectException("Failed to connect to /192.168.1.10:32400"), true),
        )
        assertEquals(
            copyFor(ConnectionFailure.INSECURE, null).short,
            transferFailureMessage(SSLHandshakeException("Trust anchor not found"), true),
        )
        assertEquals(
            copyFor(ConnectionFailure.OFFLINE, null).short,
            transferFailureMessage(ConnectException("Network is unreachable"), false),
        )
    }

    /**
     * An `IOException` the taxonomy does not name keeps the message it had
     * before the taxonomy existed. Guessing `unreachable` for it is exactly
     * the mistake that produced "Can't reach the server." for a full disk.
     */
    @Test
    fun anUnnamedFailureIsNotGuessedAtAsUnreachable() {
        val opaque = IOException("Response code: 416")
        assertEquals("Response code: 416", transferFailureMessage(opaque, true))
        assertEquals(
            "Media3 said something we do not model",
            transferFailureMessage(IllegalStateException("Media3 said something we do not model"), true),
        )
    }

    @Test
    fun aCancelledTransferSaysNothingAtAll() {
        assertNull(transferFailureMessage(CancellationException("cancelled"), true))
        assertNull(
            transferFailureMessage(IOException("wrapped", CancellationException("cancelled")), true),
        )
    }

    /**
     * Both paths that write `OfflineRecord.errorMessage` render on the same
     * line of `DownloadsScreen`, so they must interpolate `{server}` the same
     * way. The transfer callback has no origin to offer — a download outlives
     * the session that queued it — so both use the contract's fallback.
     */
    @Test
    fun bothFailurePathsNameTheServerIdentically() {
        assertEquals(
            copyFor(ConnectionFailure.UNKNOWN_HOST, null).short,
            transferFailureMessage(UnknownHostException("plurx.local"), true),
        )
        assertEquals(
            "Can't find the server.",
            transferFailureMessage(UnknownHostException("plurx.local"), true),
        )
    }
}
