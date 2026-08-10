@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import androidx.media3.exoplayer.hls.playlist.HlsMultivariantPlaylist
import androidx.media3.exoplayer.offline.Download
import android.app.job.JobParameters
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

    @Test
    fun coldStartOnlyAddsUidtOwnershipToAnUnstoppedTransfer() {
        assertEquals(
            OfflineDownloads.UIDT_WAITING_REASON,
            coldStartStopReason(Download.STOP_REASON_NONE),
        )
        assertEquals(
            OfflineDownloads.SYSTEM_TIMEOUT_REASON,
            coldStartStopReason(OfflineDownloads.SYSTEM_TIMEOUT_REASON),
        )
        assertEquals(
            OfflineDownloads.SYSTEM_INTERRUPTED_REASON,
            coldStartStopReason(OfflineDownloads.SYSTEM_INTERRUPTED_REASON),
        )
        assertEquals(
            OfflineDownloads.SYSTEM_INTERRUPTED_REASON,
            coldStartStopReason(
                Download.STOP_REASON_NONE,
                TransferRecoveryState.PausedBySystem,
            ),
        )
    }

    @Test
    fun staleAndRegressiveTransferCallbacksCannotOverwriteDurableUiState() {
        val base = OfflineRecord(
            id = "local-1",
            requestId = "request-1",
            serverInstanceId = "server-1",
            userId = 7,
            itemId = 11,
            fileId = 13,
            title = "Flight",
            requestedHeight = 720,
            state = "downloading",
            transferSequence = 20,
        )
        fun snapshot(state: Int, stopReason: Int = 0, sequence: Long = 21) = DownloadSnapshot(
            id = base.id,
            state = state,
            stopReason = stopReason,
            bytesDownloaded = 10,
            contentLength = 100,
            percentDownloaded = 10f,
            errorMessage = null,
            sequence = sequence,
        )

        assertEquals(false, shouldApplyTransferSnapshot(base, snapshot(Download.STATE_QUEUED, sequence = 19)))
        assertEquals(
            false,
            shouldApplyTransferSnapshot(
                base.copy(state = "completed"),
                snapshot(Download.STATE_DOWNLOADING),
            ),
        )
        assertEquals(
            false,
            shouldApplyTransferSnapshot(
                base.copy(state = "paused", phase = "paused_by_system"),
                snapshot(Download.STATE_DOWNLOADING),
            ),
        )
        assertEquals(
            true,
            shouldApplyTransferSnapshot(
                base.copy(state = "paused", phase = "paused_by_system"),
                snapshot(
                    Download.STATE_STOPPED,
                    stopReason = OfflineDownloads.SYSTEM_INTERRUPTED_REASON,
                ),
            ),
        )
    }

    @Test
    fun onlyTaskManagerAndTimeoutStopsRequireAnExplicitResume() {
        assertEquals(true, jobStopRequiresExplicitResume(JobParameters.STOP_REASON_USER))
        assertEquals(true, jobStopRequiresExplicitResume(JobParameters.STOP_REASON_TIMEOUT))
        assertEquals(true, jobStopRequiresExplicitResume(JobParameters.STOP_REASON_TIMEOUT_ABANDONED))
        assertEquals(false, jobStopRequiresExplicitResume(JobParameters.STOP_REASON_CONSTRAINT_CONNECTIVITY))
        assertEquals(false, jobStopRequiresExplicitResume(JobParameters.STOP_REASON_PREEMPT))
    }
}
