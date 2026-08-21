@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data

import android.content.Context
import android.hardware.display.DisplayManager
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.os.Build
import android.util.Log
import android.view.Display
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.exoplayer.audio.AudioCapabilities
import androidx.media3.exoplayer.mediacodec.MediaCodecUtil
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import tv.plurx.app.BuildConfig

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
    private val VIDEO_CODEC_MIMES = mapOf(
        "video/avc" to "h264",
        "video/hevc" to "hevc",
        "video/av01" to "av1",
        "video/x-vnd.on2.vp9" to "vp9",
    )
    private val VIDEO_PROBE_SIZES = listOf(
        3840 to 2160,
        2560 to 1440,
        1920 to 1080,
        1280 to 720,
        854 to 480,
        640 to 360,
    )

    suspend fun query(context: Context): Map<String, String> = withContext(Dispatchers.IO) {
        probe(context)
    }

    private fun probe(context: Context): Map<String, String> {
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
        val videoLimits = videoDecoderLimits(codecs).toMutableList()
        // AVC decoding is mandatory on Android. If a vendor registry is
        // temporarily unreadable, retain that baseline conservatively instead
        // of restoring the old, dangerous "codec present means uncapped 4K"
        // claim.
        if (videoLimits.none { it.codec == "h264" }) {
            videoLimits += VideoDecoderLimit("h264", 1080)
        }
        val video = videoCodecCaps(videoLimits)

        val audio = audioCodecClaims(decoderMimes, sinkEncodings(context))

        val hdrTypes = displayHdrTypes(context)
        val hdr = displayIsHdr(hdrTypes) &&
            ("hevc" in video.codecs || "av1" in video.codecs)
        // Ask the same Media3 decoder selector that ExoPlayer uses. The raw
        // platform registry can omit aliases and device workarounds that
        // Media3 applies at playback time, which made capable Google TV boxes
        // under-report Dolby Vision and receive an SDR transcode instead.
        val dolbyVisionDecoderProbe = decoderDolbyVisionProbe()
        val rawDolbyVisionProfiles = dolbyVisionDecoderProbe.profiles
        val dolbyVisionProfiles = dolbyVisionProfiles(rawDolbyVisionProfiles)
        val dolbyVision = dolbyVisionCaps(
            profiles = dolbyVisionProfiles,
            displaySupportsDolbyVision = HdrType.DOLBY_VISION in hdrTypes,
        )
        val diagnostics = capabilityDiagnostics(
            version = "${BuildConfig.VERSION_NAME}(${BuildConfig.VERSION_CODE})",
            hdrTypes = hdrTypes,
            decoderNames = dolbyVisionDecoderProbe.names,
            rawProfiles = rawDolbyVisionProfiles,
            claimedProfiles = dolbyVisionProfiles,
        )

        val result = video.queryParams() + mapOf(
            "client" to "android",
            "device" to Build.MODEL,
            "acodec" to audio.joinToString(","),
            // Media3's progressive extractors handle the audiobook containers
            // too. Leaving them out routes audio-only sources through the
            // video HLS machinery even when this device can play them raw.
            "container" to DIRECT_PLAY_CONTAINERS,
            "hdr" to if (hdr) "1" else "0",
        ) + dolbyVision + diagnostics
        Log.i(
            LOG_TAG,
            "model=${Build.MODEL} hdrTypes=${hdrTypes.sorted()} " +
                "dvDecoders=${dolbyVisionDecoderProbe.names} " +
                "rawDvProfiles=${rawDolbyVisionProfiles.sorted()} " +
                "claimedDvProfiles=$dolbyVisionProfiles caps=$result",
        )
        return result
    }

    /**
     * Highest ordinary movie frame each registered decoder proves at 30 fps.
     * `supportedHeights.upper` alone is not enough: it may describe a narrow
     * frame that the decoder cannot sustain at the corresponding 16:9 width.
     */
    private fun videoDecoderLimits(codecs: Array<MediaCodecInfo>): List<VideoDecoderLimit> =
        buildList {
            codecs.filterNot { it.isEncoder }.forEach { info ->
                info.supportedTypes.forEach typeLoop@{ advertisedType ->
                    val codec = VIDEO_CODEC_MIMES[advertisedType.lowercase()]
                        ?: return@typeLoop
                    val video = try {
                        info.getCapabilitiesForType(advertisedType).videoCapabilities
                    } catch (_: Exception) {
                        null
                    } ?: return@typeLoop
                    val maxHeight = VIDEO_PROBE_SIZES.firstOrNull { (width, height) ->
                        try {
                            video.areSizeAndRateSupported(width, height, 30.0)
                        } catch (_: Exception) {
                            false
                        }
                    }?.second ?: return@typeLoop
                    add(VideoDecoderLimit(codec, maxHeight))
                }
            }
        }

    private data class DolbyVisionDecoderProbe(
        val names: List<String>,
        val profiles: List<Int>,
    )

    /** Non-secure, non-tunneled Dolby Vision decoders Media3 can actually select. */
    private fun decoderDolbyVisionProbe(): DolbyVisionDecoderProbe = try {
        val decoders = MediaCodecUtil.getDecoderInfos(
            DOLBY_VISION_MIME,
            /* secure = */ false,
            /* tunneling = */ false,
        )
        DolbyVisionDecoderProbe(
            names = decoders.map { it.name }.distinct(),
            profiles = decoders.flatMap { decoder ->
                decoder.profileLevels.map { it.profile }
            }.distinct(),
        )
    } catch (_: Exception) {
        DolbyVisionDecoderProbe(emptyList(), emptyList())
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
