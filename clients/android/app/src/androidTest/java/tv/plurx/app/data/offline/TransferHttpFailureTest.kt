@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import android.net.Uri
import androidx.media3.datasource.DataSpec
import androidx.media3.datasource.HttpDataSource.InvalidResponseCodeException
import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.ConnectionFailure
import tv.plurx.app.data.copyFor

/**
 * The one branch of [transferFailureMessage] a JVM test cannot reach:
 * `InvalidResponseCodeException` needs a `DataSpec`, which needs `android.net.Uri`.
 *
 * It matters because "the server answered" is the opposite of "can't reach the
 * server", and both used to render the latter.
 */
class TransferHttpFailureTest {

    private fun answered(code: Int, message: String) = InvalidResponseCodeException(
        code,
        message,
        null,
        emptyMap(),
        DataSpec(Uri.parse("http://192.168.1.10:32400/offline/packages/abc/manifest.m3u8")),
        ByteArray(0),
    )

    @Test
    fun aServerErrorDuringTransferIsTheServerErrorClass() {
        assertEquals(
            copyFor(ConnectionFailure.SERVER_ERROR, null).short,
            transferFailureMessage(answered(503, "Service Unavailable"), true),
        )
    }

    /**
     * A 404 (the package expired server-side) or a 401 is something specific
     * the server said. The taxonomy has no class for either, so its own message
     * stands rather than a guess — and in particular never "Can't reach", which
     * would be flatly untrue of a server that just answered.
     */
    @Test
    fun anAnsweredFourHundredKeepsItsOwnMessageAndNeverSaysUnreachable() {
        for (code in listOf(401, 403, 404, 410)) {
            val message = transferFailureMessage(answered(code, "nope"), true).orEmpty()
            assertEquals(
                "code $code must not be reported as unreachable",
                false,
                message == copyFor(ConnectionFailure.UNREACHABLE, null).short,
            )
        }
    }
}
