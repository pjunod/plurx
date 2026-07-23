package tv.plurx.app.data

import android.content.Context
import android.hardware.display.DisplayManager
import android.media.MediaCodecList
import android.os.Build
import android.view.Display

/**
 * Runtime playback capabilities for this device, sent to `/decision` so the
 * server only transcodes what this hardware can't play. Android's advantage
 * over browsers: ExoPlayer direct-plays MKV/TS and the SoC decoders handle
 * HEVC/AV1/DTS on capable devices — so most files come back direct-play or a
 * cheap copy-remux. HDR is only claimed on an HDR-capable display, matching the
 * server's tone-map-on-SDR rule.
 */
object Caps {

    fun query(context: Context): Map<String, String> {
        val video = linkedSetOf("h264")
        val audio = linkedSetOf("aac", "mp3", "opus", "flac")

        val codecs = try {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
        } catch (_: Exception) {
            emptyArray()
        }
        fun decodes(mime: String): Boolean = codecs.any { info ->
            !info.isEncoder && info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
        }

        if (decodes("video/hevc")) video.add("hevc")
        if (decodes("video/av01")) video.add("av1")
        if (decodes("video/x-vnd.on2.vp9")) video.add("vp9")
        if (decodes("audio/ac3")) audio.add("ac3")
        if (decodes("audio/eac3")) audio.add("eac3")
        if (decodes("audio/vnd.dts") || decodes("audio/vnd.dts.hd")) audio.add("dts")
        if (decodes("audio/true-hd")) audio.add("truehd")

        val hdr = displayIsHdr(context) && (video.contains("hevc") || video.contains("av1"))

        return mapOf(
            "vcodec" to video.joinToString(","),
            "acodec" to audio.joinToString(","),
            // ExoPlayer plays these containers natively (MKV/TS included).
            "container" to "mkv,mp4,webm,mov,ts",
            "hdr" to if (hdr) "1" else "0",
        )
    }

    @Suppress("DEPRECATION") // Display.getHdrCapabilities: fine as a coarse HDR probe here.
    private fun displayIsHdr(context: Context): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.N) return false
        return try {
            val dm = context.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
            val display = dm.getDisplay(Display.DEFAULT_DISPLAY)
            val caps = display?.hdrCapabilities
            caps?.supportedHdrTypes?.isNotEmpty() == true
        } catch (_: Exception) {
            false
        }
    }
}
