package tv.plurx.app.data

import android.content.Context
import android.media.MediaCodecList
import android.os.Build
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Physical-device coverage for the capability probe that controls whether the
 * server preserves Dolby Vision/HDR or falls back to SDR. Generic emulators
 * exercise the invariants; known hardware also pins its advertised contract so
 * a framework or client change cannot silently downgrade it.
 */
@RunWith(AndroidJUnit4::class)
class CapabilityProbeTest {
    private val context: Context
        get() = InstrumentationRegistry.getInstrumentation().targetContext

    @Test
    fun runtimeCapabilitiesAreSelfConsistent() = runBlocking {
        val caps = Caps.query(context)
        println("PLURX_CAPABILITIES model=${Build.MODEL} caps=$caps")

        assertNotNull(caps["vcodec"])
        assertNotNull(caps["acodec"])
        assertNotNull(caps["container"])
        assertTrue(caps["hdr"] == "0" || caps["hdr"] == "1")
        if (caps["dv"] == "1") {
            assertEquals("1", caps["hdr"])
            assertTrue(caps["dvprofile"].orEmpty().isNotBlank())
        }
    }

    @Test
    fun lenovoLegionTabAdvertisesItsDolbyVisionPipeline() = runBlocking {
        if (Build.MODEL != "TB322FC") return@runBlocking

        val displayTypes = Caps.displayHdrTypes(context)
        val decoderTypes = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
            .filterNot { it.isEncoder }
            .flatMap { it.supportedTypes.asIterable() }
            .map(String::lowercase)
            .toSet()
        val caps = Caps.query(context)
        println(
            "PLURX_LENOVO_CAPABILITIES display=$displayTypes " +
                "decoderTypes=${decoderTypes.sorted()} caps=$caps",
        )

        assertTrue(HdrType.DOLBY_VISION in displayTypes)
        assertTrue("video/dolby-vision" in decoderTypes)
        assertEquals("1", caps["hdr"])
        assertEquals("1", caps["dv"])
        assertTrue(caps["dvprofile"].orEmpty().isNotBlank())
    }
}
