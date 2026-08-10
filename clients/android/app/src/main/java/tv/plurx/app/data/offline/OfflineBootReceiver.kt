@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.media3.exoplayer.offline.DownloadService

/** Legacy boot catch-up; API 34+ recovery is owned by persisted UIDT jobs. */
class OfflineBootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) return
        if (intent.action != Intent.ACTION_BOOT_COMPLETED && intent.action != MEDIA3_RESTART) return
        OfflineDownloads.initialize(context)
        DownloadService.sendResumeDownloads(
            context,
            PlurxDownloadService::class.java,
            true,
        )
    }

    companion object {
        // Media3 keeps this constant private, but PlatformScheduler emits this
        // exact explicit action when its persisted constraints become met.
        const val MEDIA3_RESTART = "androidx.media3.exoplayer.downloadService.action.RESTART"
    }
}
