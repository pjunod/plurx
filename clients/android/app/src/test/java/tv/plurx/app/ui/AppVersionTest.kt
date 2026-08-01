package tv.plurx.app.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class AppVersionTest {
    @Test
    fun packageVersionIncludesItsBuildNumber() {
        assertEquals("0.2.0 (2)", appVersionLabel("0.2.0", 2))
    }
}
