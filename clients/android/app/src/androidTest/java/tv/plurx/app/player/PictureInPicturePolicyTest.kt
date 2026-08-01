package tv.plurx.app.player

import android.util.Rational
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class PictureInPicturePolicyTest {
    @Test
    fun landscapeVideoUsesItsDisplayAspectRatio() {
        assertEquals(Rational(16, 9), calculatePipAspectRatio(1920, 1080))
    }

    @Test
    fun anamorphicVideoIncludesPixelAspectRatio() {
        assertEquals(Rational(900, 576), calculatePipAspectRatio(720, 576, 1.25f))
    }

    @Test
    fun portraitVideoKeepsItsPortraitShape() {
        assertEquals(Rational(9, 16), calculatePipAspectRatio(1080, 1920))
    }

    @Test
    fun unsupportedOrInvalidRatiosFallBackToWidescreen() {
        assertEquals(Rational(16, 9), calculatePipAspectRatio(4000, 500))
        assertEquals(Rational(16, 9), calculatePipAspectRatio(0, 0))
        assertEquals(Rational(16, 9), calculatePipAspectRatio(1920, 1080, Float.NaN))
    }
}
