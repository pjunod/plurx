@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import androidx.media3.exoplayer.hls.playlist.HlsMultivariantPlaylist
import androidx.media3.exoplayer.offline.Download
import android.app.job.JobParameters
import android.app.job.JobInfo
import androidx.media3.exoplayer.scheduler.Requirements
import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.OfflineNetwork
import java.io.File

class OfflineDownloadsTest {
    @Test
    fun offlinePlayerCannotBypassApplicationLooperDownloadAccess() {
        val candidates = listOf(
            File("app/src/main/java/tv/plurx/app/player/OfflinePlayerScreen.kt"),
            File("src/main/java/tv/plurx/app/player/OfflinePlayerScreen.kt"),
            File("clients/android/app/src/main/java/tv/plurx/app/player/OfflinePlayerScreen.kt"),
        )
        val source = candidates.firstOrNull(File::isFile)
            ?: error("OfflinePlayerScreen.kt source not found")
        assertEquals(false, source.readText().contains("OfflineDownloads.manager"))
    }

    @Test
    fun uidtFastFailureStartsOnlyAfterTheFrameworkCallbackReturns() {
        val candidates = listOf(
            File("app/src/main/java/tv/plurx/app/data/offline/OfflineTransferJobService.kt"),
            File("src/main/java/tv/plurx/app/data/offline/OfflineTransferJobService.kt"),
            File("clients/android/app/src/main/java/tv/plurx/app/data/offline/OfflineTransferJobService.kt"),
        )
        val source = candidates.firstOrNull(File::isFile)?.readText()
            ?: error("OfflineTransferJobService.kt source not found")
        assertEquals(true, source.contains("SupervisorJob() + Dispatchers.Main)"))
        assertEquals(false, source.contains("SupervisorJob() + Dispatchers.Main.immediate"))
        assertEquals(true, source.contains("start = CoroutineStart.LAZY"))
        assertEquals(
            true,
            source.indexOf("running[parameters.jobId] =") < source.indexOf("job.start()"),
        )
    }

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
    fun synchronousRecoveryPolicyWinsTheDataStoreCrashWindow() {
        assertEquals(
            OfflineNetwork.WifiOnly,
            authoritativeOfflineNetwork(OfflineNetwork.WifiOnly, OfflineNetwork.Any),
        )
        assertEquals(
            OfflineNetwork.Any,
            authoritativeOfflineNetwork(OfflineNetwork.Any, OfflineNetwork.WifiOnly),
        )
    }

    @Test
    fun uidtJobAndGrantedNetworkFollowBothPolicyTransitions() {
        assertEquals(JobInfo.NETWORK_TYPE_ANY, uidtNetworkType(OfflineNetwork.Any))
        assertEquals(JobInfo.NETWORK_TYPE_UNMETERED, uidtNetworkType(OfflineNetwork.WifiOnly))
        assertEquals(true, networkSatisfiesPolicy(OfflineNetwork.Any, isUnmetered = false))
        assertEquals(true, networkSatisfiesPolicy(OfflineNetwork.Any, isUnmetered = true))
        assertEquals(false, networkSatisfiesPolicy(OfflineNetwork.WifiOnly, isUnmetered = false))
        assertEquals(true, networkSatisfiesPolicy(OfflineNetwork.WifiOnly, isUnmetered = true))

        val candidates = listOf(
            File("app/src/main/java/tv/plurx/app/data/offline/OfflineDownloads.kt"),
            File("src/main/java/tv/plurx/app/data/offline/OfflineDownloads.kt"),
            File("clients/android/app/src/main/java/tv/plurx/app/data/offline/OfflineDownloads.kt"),
        )
        val source = candidates.firstOrNull(File::isFile)?.readText()
            ?: error("OfflineDownloads.kt source not found")
        val reconfigure = source.substringAfter("private fun reconfigureUidtTransfers")
            .substringBefore("fun currentNetworkPolicy")
        assertEquals(true, reconfigure.contains("awaitStopReason(record.id, UIDT_WAITING_REASON)"))
        assertEquals(
            true,
            reconfigure.indexOf("awaitStopReason(") <
                reconfigure.indexOf("OfflineTransferJobService.schedule("),
        )
        assertEquals(
            true,
            reconfigure.indexOf("networkPolicyGeneration.get() != policyGeneration") <
                reconfigure.indexOf("OfflineTransferJobService.schedule("),
        )
    }

    @Test
    fun rebindingCancelsOldCallsAndStaleOwnerCannotClearReplacement() {
        var oldCancellations = 0
        var newCancellations = 0
        val bindings = mutableMapOf<String, GenerationBoundCancellation>()
        val old = GenerationBoundCancellation(owner = 41) { oldCancellations++ }
        val replacement = GenerationBoundCancellation(owner = 42) { newCancellations++ }
        replaceOwnedBinding(bindings, "download-1", old) { it.revokeCurrent() }
        replaceOwnedBinding(bindings, "download-1", replacement) { it.revokeCurrent() }

        assertEquals(1, oldCancellations)
        assertEquals(false, checkNotNull(bindings["download-1"]).revoke(requester = 41))
        assertEquals(0, newCancellations)
        assertEquals(true, checkNotNull(bindings["download-1"]).revoke(requester = 42))
        assertEquals(1, newCancellations)
        val gate = RevocableNetworkGate()
        assertEquals(true, gate.isOpen())
        gate.revoke()
        assertEquals(false, gate.isOpen())
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
    fun oneResumeTapAuthorizesOnlyItsOwnRow() {
        assertEquals(true, isExplicitResumeTarget("download-a", "download-a"))
        assertEquals(false, isExplicitResumeTarget("download-b", "download-a"))
        assertEquals(false, isExplicitResumeTarget("download-a", null))
        assertEquals(
            OfflineDownloads.UIDT_WAITING_REASON,
            explicitResumeStopReason(
                OfflineDownloads.SYSTEM_TIMEOUT_REASON,
                isTappedRow = true,
            ),
        )
        assertEquals(
            OfflineDownloads.UIDT_WAITING_REASON,
            explicitResumeStopReason(
                OfflineDownloads.SYSTEM_INTERRUPTED_REASON,
                isTappedRow = true,
            ),
        )
        assertEquals(
            OfflineDownloads.UIDT_WAITING_REASON,
            explicitResumeStopReason(
                Download.STOP_REASON_NONE,
                isTappedRow = true,
            ),
        )
        assertEquals(
            OfflineDownloads.SYSTEM_TIMEOUT_REASON,
            explicitResumeStopReason(
                OfflineDownloads.SYSTEM_TIMEOUT_REASON,
                isTappedRow = false,
            ),
        )

        val candidates = listOf(
            File("app/src/main/java/tv/plurx/app/data/offline/OfflineDownloads.kt"),
            File("src/main/java/tv/plurx/app/data/offline/OfflineDownloads.kt"),
            File("clients/android/app/src/main/java/tv/plurx/app/data/offline/OfflineDownloads.kt"),
        )
        val source = candidates.firstOrNull(File::isFile)?.readText()
            ?: error("OfflineDownloads.kt source not found")
        val resumeTransition = source.indexOf("awaitStopReason(record.id, waitingReason)")
        val jobRegistration = source.indexOf("OfflineTransferJobService.schedule(", resumeTransition)
        assertEquals(true, resumeTransition >= 0)
        assertEquals(true, jobRegistration > resumeTransition)
    }

    @Test
    fun upgradeAndRegistrationFailuresExposeARealResumeAction() {
        fun record(state: String, phase: String) = OfflineRecord(
            id = "local-1",
            requestId = "request-1",
            serverInstanceId = "server-1",
            userId = 7,
            itemId = 11,
            fileId = 13,
            title = "Flight",
            requestedHeight = 720,
            state = state,
            phase = phase,
        )
        assertEquals(true, record("ready", "waiting_for_transfer_job").needsExplicitResume)
        assertEquals(true, record("ready", "waiting_for_foreground").needsExplicitResume)
        assertEquals(true, record("paused", "paused_by_system").needsExplicitResume)
        assertEquals(false, record("ready", "ready_to_download").needsExplicitResume)
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
