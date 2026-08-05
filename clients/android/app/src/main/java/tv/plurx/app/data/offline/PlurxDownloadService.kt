@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import android.app.Notification
import androidx.media3.exoplayer.offline.Download
import androidx.media3.exoplayer.offline.DownloadManager
import androidx.media3.exoplayer.offline.DownloadNotificationHelper
import androidx.media3.exoplayer.offline.DownloadService
import androidx.media3.exoplayer.scheduler.PlatformScheduler
import androidx.media3.exoplayer.scheduler.Scheduler
import tv.plurx.app.R

class PlurxDownloadService : DownloadService(
    NOTIFICATION_ID,
    DEFAULT_FOREGROUND_NOTIFICATION_UPDATE_INTERVAL,
    CHANNEL_ID,
    R.string.offline_download_channel,
    0,
) {
    private val notifications by lazy { DownloadNotificationHelper(this, CHANNEL_ID) }

    override fun getDownloadManager(): DownloadManager = OfflineDownloads.manager

    override fun getScheduler(): Scheduler = PlatformScheduler(this, SCHEDULER_JOB_ID)

    override fun getForegroundNotification(
        downloads: MutableList<Download>,
        notMetRequirements: Int,
    ): Notification = notifications.buildProgressNotification(
        this,
        android.R.drawable.stat_sys_download,
        null,
        "Downloading for offline viewing",
        downloads,
        notMetRequirements,
    )

    override fun onTimeout(startId: Int, fgsType: Int) {
        OfflineDownloads.manager.currentDownloads.forEach {
            OfflineDownloads.manager.setStopReason(it.request.id, OfflineDownloads.SYSTEM_TIMEOUT_REASON)
        }
        stopSelf(startId)
    }

    companion object {
        private const val CHANNEL_ID = "offline-downloads"
        private const val NOTIFICATION_ID = 2_031
        private const val SCHEDULER_JOB_ID = 2_032
    }
}
