package tv.plurx.app.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewerPreferencesTest {
    @Test
    fun persistedEnumsRoundTripEverySupportedValue() {
        ThemeId.entries.forEach { assertEquals(it, ThemeId.fromStorage(it.storageValue)) }
        Appearance.entries.forEach { assertEquals(it, Appearance.fromStorage(it.storageValue)) }
        PosterSize.entries.forEach { assertEquals(it, PosterSize.fromStorage(it.storageValue)) }
        HomeGrouping.entries.forEach { assertEquals(it, HomeGrouping.fromStorage(it.storageValue)) }
        PlaybackQuality.entries.forEach { assertEquals(it, PlaybackQuality.fromStorage(it.storageValue)) }
        OfflineQuality.entries.forEach { assertEquals(it, OfflineQuality.fromStorage(it.storageValue)) }
        OfflineNetwork.entries.forEach { assertEquals(it, OfflineNetwork.fromStorage(it.storageValue)) }
    }

    @Test
    fun corruptPreferencesFallBackToSafeViewerDefaults() {
        assertEquals(ThemeId.Classic, ThemeId.fromStorage("unknown"))
        assertEquals(Appearance.System, Appearance.fromStorage("unknown"))
        assertEquals(PosterSize.Medium, PosterSize.fromStorage("unknown"))
        assertEquals(HomeGrouping.Category, HomeGrouping.fromStorage("unknown"))
        assertEquals(PlaybackQuality.Auto, PlaybackQuality.fromStorage("unknown"))
        assertEquals(OfflineQuality.Standard, OfflineQuality.fromStorage("unknown"))
        assertEquals(OfflineNetwork.WifiOnly, OfflineNetwork.fromStorage("unknown"))
    }

    @Test
    fun playbackDefaultsMatchTheWebViewer() {
        val defaults = ViewerPreferences()
        assertFalse(defaults.autoSkip)
        assertTrue(defaults.autoplayNext)
        assertEquals(PlaybackQuality.Auto, defaults.playbackQuality)
        assertEquals(OfflineQuality.Standard, defaults.offlineQuality)
        assertEquals(OfflineNetwork.WifiOnly, defaults.offlineNetwork)
    }
}
