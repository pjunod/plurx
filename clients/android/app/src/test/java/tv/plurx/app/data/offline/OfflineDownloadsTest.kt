@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import androidx.media3.exoplayer.hls.playlist.HlsMultivariantPlaylist
import androidx.media3.exoplayer.scheduler.Requirements
import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.OfflineNetwork

class OfflineDownloadsTest {
    @Test
    fun restoredNetworkPolicyKeepsTheSavedTransportRequirement() {
        assertEquals(
            Requirements.NETWORK or Requirements.NETWORK_UNMETERED or
                Requirements.DEVICE_STORAGE_NOT_LOW,
            offlineRequirements(OfflineNetwork.WifiOnly).requirements,
        )
        assertEquals(
            Requirements.NETWORK or Requirements.DEVICE_STORAGE_NOT_LOW,
            offlineRequirements(OfflineNetwork.Any).requirements,
        )
    }

    @Test
    fun subtitleSelectionUsesTheHlsSubtitleGroup() {
        val keys = offlineStreamKeys(hasSubtitle = true)
        assertEquals(HlsMultivariantPlaylist.GROUP_INDEX_VARIANT, keys[0].groupIndex)
        assertEquals(HlsMultivariantPlaylist.GROUP_INDEX_SUBTITLE, keys[1].groupIndex)
    }

    @Test
    fun aRestoredCompletedTransferStillReleasesItsServerPackage() {
        fun record(state: String, packageId: String? = "package-1") = OfflineRecord(
            id = "local-1",
            requestId = "request-1",
            serverInstanceId = "server-1",
            userId = 7,
            itemId = 11,
            fileId = 13,
            title = "Flight",
            requestedHeight = 720,
            packageId = packageId,
            state = state,
        )

        assertEquals(true, record("completed").needsServerCompletion)
        assertEquals(false, record("downloading").needsServerCompletion)
        assertEquals(false, record("completed", packageId = null).needsServerCompletion)
    }
}
