package tv.plurx.app.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class AppViewModelTest {
    @Test
    fun manualOriginsUseThePlurxPortWhenHttpHasNone() {
        assertEquals("http://192.168.1.20:32400", normalizeOrigin("192.168.1.20"))
        assertEquals("http://media-box:32400", normalizeOrigin("media-box"))
        assertEquals("http://media-box:32400", normalizeOrigin("http://media-box"))
    }

    @Test
    fun manualOriginsPreserveExplicitPortsAndHttpsDefaults() {
        assertEquals("http://media-box:32500", normalizeOrigin("media-box:32500"))
        assertEquals("https://media.example.test", normalizeOrigin("https://media.example.test/"))
        assertEquals("", normalizeOrigin("   "))
    }
}
