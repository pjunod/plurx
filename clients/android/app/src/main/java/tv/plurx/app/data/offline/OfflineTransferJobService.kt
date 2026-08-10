@file:androidx.annotation.RequiresApi(34)

package tv.plurx.app.data.offline

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.os.PersistableBundle
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import tv.plurx.app.R
import tv.plurx.app.data.OfflineNetwork
import java.util.concurrent.atomic.AtomicLong

/** Android 14+ owner for user-requested, persisted offline transfers. */
class OfflineTransferJobService : JobService() {
    private data class RunningTransfer(val downloadId: String, val owner: Long, val job: Job)

    // Non-immediate Main is intentional: a fast missing/stopped row must not
    // call jobFinished before onStartJob has returned true to the framework.
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private val running = mutableMapOf<Int, RunningTransfer>()

    override fun onStartJob(parameters: JobParameters): Boolean {
        val id = parameters.extras.getString(EXTRA_DOWNLOAD_ID) ?: return false
        val network = parameters.network
        if (network == null) {
            OfflineDownloads.pauseForSystem(id, OfflineDownloads.SYSTEM_INTERRUPTED_REASON)
            return false
        }
        if (!OfflineDownloads.uidtNetworkSatisfiesCurrentPolicy(network)) {
            OfflineDownloads.pauseForJobRetry(id)
            return false
        }
        ensureChannel()
        setNotification(
            parameters,
            parameters.jobId,
            NotificationCompat.Builder(this, CHANNEL_ID)
                .setSmallIcon(android.R.drawable.stat_sys_download)
                .setContentTitle(getString(R.string.app_name))
                .setContentText("Downloading for offline viewing")
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .build(),
            JOB_END_NOTIFICATION_POLICY_REMOVE,
        )
        val owner = NEXT_OWNER.getAndIncrement()
        val job = scope.launch(start = CoroutineStart.LAZY) {
            var finishNormally = false
            try {
                OfflineDownloads.runUserInitiatedTransfer(id, network, owner)
                finishNormally = true
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: Exception) {
                // Fail closed. A corrupt index, persistence failure, or other
                // unexpected owner error must not leave a scheduler wakelock
                // and notification alive until timeout.
                runCatching {
                    OfflineDownloads.pauseForSystem(
                        id,
                        OfflineDownloads.SYSTEM_INTERRUPTED_REASON,
                    )
                }
                finishNormally = true
            } finally {
                val current = running[parameters.jobId]
                if (current?.owner == owner) {
                    running.remove(parameters.jobId)
                    if (finishNormally) {
                        // Constraint loss reaches onStopJob instead. Terminal,
                        // missing, failed, and explicit-pause rows do not retry.
                        jobFinished(parameters, false)
                    }
                }
            }
        }
        running[parameters.jobId] = RunningTransfer(id, owner, job)
        job.start()
        return true
    }

    override fun onNetworkChanged(parameters: JobParameters) {
        val id = parameters.extras.getString(EXTRA_DOWNLOAD_ID) ?: return
        val active = running[parameters.jobId] ?: return
        parameters.network?.let { network ->
            OfflineDownloads.replaceUidtNetwork(id, network, active.owner)
        }
    }

    override fun onStopJob(parameters: JobParameters): Boolean {
        val id = parameters.extras.getString(EXTRA_DOWNLOAD_ID) ?: return false
        val active = running.remove(parameters.jobId)
        // Revoke sockets synchronously before returning UIDT ownership to the
        // system. Generation matching prevents an old job from touching a
        // replacement that already owns the same download id.
        active?.let { OfflineDownloads.revokeUidtNetwork(id, it.owner) }
        active?.job?.cancel()
        if (parameters.stopReason == JobParameters.STOP_REASON_CANCELLED_BY_APP) return false
        if (jobStopRequiresExplicitResume(parameters.stopReason)) {
            val reason = if (
                parameters.stopReason == JobParameters.STOP_REASON_TIMEOUT ||
                parameters.stopReason == JobParameters.STOP_REASON_TIMEOUT_ABANDONED
            ) {
                OfflineDownloads.SYSTEM_TIMEOUT_REASON
            } else {
                OfflineDownloads.SYSTEM_INTERRUPTED_REASON
            }
            OfflineDownloads.pauseForSystem(id, reason)
            return false
        }
        // Connectivity/storage/preemption stops retain the persisted job and
        // the request's UIDT ownership. Android retries it when constraints
        // permit; only a user/timeout stop demands another tap.
        OfflineDownloads.pauseForJobRetry(id)
        return true
    }

    override fun onDestroy() {
        running.values.forEach { transfer ->
            OfflineDownloads.revokeUidtNetwork(transfer.downloadId, transfer.owner)
        }
        running.clear()
        scope.cancel()
        super.onDestroy()
    }

    private fun ensureChannel() {
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.offline_download_channel),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
    }

    companion object {
        private const val EXTRA_DOWNLOAD_ID = "download_id"
        private const val CHANNEL_ID = "offline-downloads"
        private val NEXT_OWNER = AtomicLong(1)
        fun schedule(
            context: Context,
            jobId: Int,
            downloadId: String,
            policy: OfflineNetwork,
            estimatedBytes: Long?,
        ): Boolean {
            val extras = PersistableBundle().apply { putString(EXTRA_DOWNLOAD_ID, downloadId) }
            val builder = JobInfo.Builder(
                jobId,
                ComponentName(context, OfflineTransferJobService::class.java),
            )
                .setUserInitiated(true)
                .setPersisted(true)
                .setRequiresStorageNotLow(true)
                .setRequiredNetworkType(uidtNetworkType(policy))
                .setExtras(extras)
            if (estimatedBytes != null && estimatedBytes > 0) {
                builder.setEstimatedNetworkBytes(estimatedBytes, 0)
            }
            return context.getSystemService(JobScheduler::class.java).schedule(builder.build()) ==
                JobScheduler.RESULT_SUCCESS
        }

        fun cancel(context: Context, jobId: Int) {
            context.getSystemService(JobScheduler::class.java).cancel(jobId)
        }
    }
}

internal fun uidtNetworkType(policy: OfflineNetwork): Int =
    if (policy == OfflineNetwork.WifiOnly) {
        JobInfo.NETWORK_TYPE_UNMETERED
    } else {
        JobInfo.NETWORK_TYPE_ANY
    }
