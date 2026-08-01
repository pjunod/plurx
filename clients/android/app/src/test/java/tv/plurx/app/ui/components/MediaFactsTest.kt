package tv.plurx.app.ui.components

import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.MediaFileDto

class MediaFactsTest {
    @Test
    fun portraitDimensionsUseTheShortEdgeAndCompactPlaybackFacts() {
        val file = MediaFileDto(
            id = 1,
            filename = "episode.mkv",
            width = 1_080,
            height = 1_920,
            video_codec = "hevc",
            hdr = "dolby_vision",
            hdr_format = "Dolby Vision Profile 8",
        )
        val audio = AudioTrack(
            index = 0,
            codec = "eac3",
            channels = 8,
            title = "Dolby Atmos",
            default = true,
        )

        val facts = playerMediaFacts(file, audio)

        assertEquals(listOf("1080P", "DV", "ATMOS 7.1"), facts.map { it.label })
        assertEquals(
            listOf(MediaFactKind.Resolution, MediaFactKind.DynamicRange, MediaFactKind.Audio),
            facts.map { it.kind },
        )
    }

    @Test
    fun detailFactsAddTheVideoCodecWithoutInventingVerticalResolution() {
        val file = MediaFileDto(
            id = 2,
            filename = "movie.mkv",
            width = 3_840,
            height = 1_608,
            video_codec = "hevc",
        )

        assertEquals(
            listOf("2160P", "HEVC"),
            detailMediaFacts(file).map { it.label },
        )
    }
}
