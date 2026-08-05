@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import androidx.media3.exoplayer.hls.playlist.HlsMultivariantPlaylist
import org.junit.Assert.assertEquals
import org.junit.Test

class OfflineDownloadsTest {
    @Test
    fun subtitleSelectionUsesTheHlsSubtitleGroup() {
        val keys = offlineStreamKeys(hasSubtitle = true)
        assertEquals(HlsMultivariantPlaylist.GROUP_INDEX_VARIANT, keys[0].groupIndex)
        assertEquals(HlsMultivariantPlaylist.GROUP_INDEX_SUBTITLE, keys[1].groupIndex)
    }
}
