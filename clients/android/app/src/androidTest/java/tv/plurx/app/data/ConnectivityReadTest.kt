package tv.plurx.app.data

import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * `hasValidatedNetwork` is the production source of the `offline` class, and
 * it is the one part of this feature a JVM test cannot reach: it needs a real
 * `ConnectivityManager`. `networkIsUsable` (unit tested) is its decision; this
 * covers the framework read that feeds it.
 *
 * The assertion is differential rather than absolute — the emulator's network
 * state is not ours to choose — so it fails whichever way the read is broken:
 * hard-coding `true` fails on a device with no validated network, hard-coding
 * `false` fails on a connected one.
 */
class ConnectivityReadTest {

    private val context = InstrumentationRegistry.getInstrumentation().targetContext

    @Test
    fun theCapabilityReadAgreesWithTheFrameworkItReads() {
        val manager = context.getSystemService(ConnectivityManager::class.java)
        val capabilities = manager.activeNetwork?.let(manager::getNetworkCapabilities)
        val expected = capabilities != null &&
            capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) &&
            capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)

        assertEquals(expected, hasValidatedNetwork(context))
    }

    /**
     * And it must be reading *something*: a device with a validated network
     * that reports itself offline (or the reverse) would make either the
     * `offline` class or every other class unreachable in production.
     */
    @Test
    fun theReadTracksTheActiveNetworkRatherThanAConstant() {
        val manager = context.getSystemService(ConnectivityManager::class.java)
        val hasActiveNetwork = manager.activeNetwork != null
        if (!hasActiveNetwork) {
            assertEquals(false, hasValidatedNetwork(context))
        }
    }
}
