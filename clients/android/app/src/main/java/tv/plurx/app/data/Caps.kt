@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data

import android.content.Context
import android.hardware.display.DisplayManager
import android.media.MediaCodecList
import android.os.Build
import android.util.Log
import android.view.Display
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.exoplayer.audio.AudioCapabilities
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/**
 * Runtime playback capabilities for this device, sent to `/decision` so the
 * server only transcodes what this hardware can't play. Android's advantage
 * over browsers: ExoPlayer direct-plays MKV/TS and the SoC decoders handle
 * HEVC/AV1/DTS on capable devices — so most files come back direct-play or a
 * cheap copy-remux. HDR (and Dolby Vision) is only claimed on a display that
 * shows it, matching the server's tone-map-on-SDR rule.
 *
 * **Recompute this per `/decision` call, never cache it.** Audio support is a
 * property of the *active route*, not the box: re-plugging HDMI, power-cycling
 * an AVR, or switching to the TV's own speakers changes the truthful answer,
 * and a stale claim over-promises a bitstream nothing downstream can decode.
 * The probes are binder IPC, which is why [query] suspends onto IO.
 */
object Caps {

    private const val DOLBY_VISION_MIME = "video/dolby-vision"
    private const val LOG_TAG = "plurx-capabilities"

    suspend fun query(context: Context): Map<String, String> = withContext(Dispatchers.IO) {
        probe(context)
    }

    private fun probe(context: Context): Map<String, String> {
        val video = linkedSetOf("h264")

        val codecs = try {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
        } catch (_: Exception) {
            emptyArray()
        }
        val decoderMimes = buildSet {
            codecs.forEach { info ->
                if (!info.isEncoder) info.supportedTypes.forEach { add(it.lowercase()) }
            }
        }

        if ("video/hevc" in decoderMimes) video.add("hevc")
        if ("video/av01" in decoderMimes) video.add("av1")
        if ("video/x-vnd.on2.vp9" in decoderMimes) video.add("vp9")

        val audio = audioCodecClaims(decoderMimes, sinkEncodings(context))

        val hdrTypes = displayHdrTypes(context)
        val hdr = displayIsHdr(hdrTypes) && (video.contains("hevc") || video.contains("av1"))
        val rawDolbyVisionProfiles = if (DOLBY_VISION_MIME in decoderMimes) {
            decoderDolbyVisionProfiles(codecs)
        } else {
            emptyList()
        }
        val dolbyVisionProfiles = dolbyVisionProfiles(rawDolbyVisionProfiles)
        val dolbyVision = dolbyVisionCaps(
            profiles = dolbyVisionProfiles,
            displaySupportsDolbyVision = HdrType.DOLBY_VISION in hdrTypes,
        )

        val result = mapOf(
            "client" to "android",
            "device" to Build.MODEL,
            "vcodec" to video.joinToString(","),
            "acodec" to audio.joinToString(","),
            // Media3's progressive extractors handle the audiobook containers
            // too. Leaving them out routes audio-only sources through the
            // video HLS machinery even when this device can play them raw.
            "container" to DIRECT_PLAY_CONTAINERS,
            "hdr" to if (hdr) "1" else "0",
        ) + dolbyVision
        Log.i(
            LOG_TAG,
            "model=${Build.MODEL} hdrTypes=${hdrTypes.sorted()} " +
                "rawDvProfiles=${rawDolbyVisionProfiles.sorted()} " +
                "claimedDvProfiles=$dolbyVisionProfiles caps=$result",
        )
        return result
    }

    /** Raw `video/dolby-vision` profile constants this device's decoders list. */
    private fun decoderDolbyVisionProfiles(
        codecs: Array<android.media.MediaCodecInfo>,
    ): List<Int> = codecs.flatMap { info ->
        if (info.isEncoder) return@flatMap emptyList()
        if (info.supportedTypes.none { it.equals(DOLBY_VISION_MIME, ignoreCase = true) }) {
            return@flatMap emptyList()
        }
        try {
            info.getCapabilitiesForType(DOLBY_VISION_MIME).profileLevels.map { it.profile }
        } catch (_: Exception) {
            emptyList()
        }
    }

    /**
     * What the *sink on the other end of the active route* accepts as a
     * bitstream. Media3 reads `ACTION_HDMI_AUDIO_PLUG` extras and, on API 33+,
     * `AudioManager.getDirectProfilesForAttributes`; passing the same
     * attributes the player uses is what makes the answer route-aware rather
     * than a blanket claim.
     */
    private fun sinkEncodings(context: Context): Set<Int> = try {
        val caps = AudioCapabilities.getCapabilities(
            context,
            AudioAttributes.Builder()
                .setUsage(C.USAGE_MEDIA)
                .setContentType(C.AUDIO_CONTENT_TYPE_MOVIE)
                .build(),
            /* routedDevice = */ null,
            // Spatializer channel masks. Empty is what the three-argument
            // overload passes; spatialization has no bearing on whether a
            // sink takes an AC3/DTS/TrueHD bitstream.
            /* spatializerChannelMasks = */ emptyList(),
        )
        setOf(
            AudioSinkEncoding.AC3,
            AudioSinkEncoding.E_AC3,
            AudioSinkEncoding.E_AC3_JOC,
            AudioSinkEncoding.DTS,
            AudioSinkEncoding.DTS_HD,
            AudioSinkEncoding.DOLBY_TRUEHD,
        ).filterTo(mutableSetOf()) { caps.supportsEncoding(it) }
    } catch (_: Exception) {
        emptySet()
    }

    /**
     * Which grades *this display* can show, as `Display.HdrCapabilities`
     * constants. The caps probe only ever needed the boolean ([displayIsHdr]),
     * but the media badges need to know whether Dolby Vision specifically is on
     * the other end of the cable — an HDR10 panel and a DV panel are both
     * "HDR" to `/decision` and mean different things on screen. Widening the
     * probe rather than adding a second one keeps the two answers from drifting;
     * the caps map it feeds is unchanged.
     */
    @Suppress("DEPRECATION") // Display.getHdrCapabilities: fine as a coarse HDR probe here.
    internal fun displayHdrTypes(context: Context): Set<Int> {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.N) return emptySet()
        return try {
            val dm = context.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
            val display = dm.getDisplay(Display.DEFAULT_DISPLAY)
            display?.hdrCapabilities?.supportedHdrTypes?.toSet().orEmpty()
        } catch (_: Exception) {
            emptySet()
        }
    }
}

internal const val DIRECT_PLAY_CONTAINERS =
    "mkv,mp4,webm,mov,ts,m4a,m4b,mp3,aac,flac,ogg,opus,wav"

/**
 * `Display.HdrCapabilities.HDR_TYPE_*`, restated so the pure badge policy can
 * read them without an Android import (they are API-24 stable constants).
 */
internal object HdrType {
    const val DOLBY_VISION = 1
    const val HDR10 = 2
    const val HLG = 3
    const val HDR10_PLUS = 4
}

/** The coarse "this panel shows some kind of HDR" answer `/decision` asks for. */
internal fun displayIsHdr(hdrTypes: Set<Int>): Boolean = hdrTypes.isNotEmpty()

/**
 * The dynamic-range vocabulary shared by `MediaFile.hdr`, the decision's
 * `delivered_dynamic_range`, and the session's — one spelling, so source and
 * delivered compare with string equality (MEDIA-BADGES-PLAN.md §3.2).
 */
internal object DynamicRange {
    const val DOLBY_VISION = "dolby_vision"
    const val HDR10 = "hdr10"
    const val HLG = "hlg"
    const val SDR = "sdr"
}
