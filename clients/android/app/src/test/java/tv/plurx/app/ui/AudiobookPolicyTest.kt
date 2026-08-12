package tv.plurx.app.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.player.audiobookGlobalPosition
import tv.plurx.app.player.nextAudiobookPartId

class AudiobookPolicyTest {
    private val audiobook = Item(id = 44, kind = "audiobook", title = "Long Book")
    private val parts = listOf(
        MediaFileDto(id = 440, filename = "Part 1.mp3", duration_ms = 60_000),
        MediaFileDto(
            id = 441,
            filename = "Part 2.mp3",
            duration_ms = 120_000,
            part_offset_ms = 60_000,
        ),
        MediaFileDto(
            id = 442,
            filename = "Part 10.mp3",
            duration_ms = 300_000,
            part_offset_ms = 180_000,
        ),
    )

    @Test
    fun textBooksRemainFirstClassNonPlayableBooks() {
        val book = Item(id = 45, kind = "book", title = "Readable")
        assertTrue(book.isBook)
    }

    @Test
    fun globalResumeSelectsTheContainingPartAndStartOverSelectsTheFirst() {
        assertEquals(440L, playbackFile(audiobook, parts, 0)?.id)
        assertEquals(440L, playbackFile(audiobook, parts, 59_999)?.id)
        assertEquals(441L, playbackFile(audiobook, parts, 60_000)?.id)
        assertEquals(442L, playbackFile(audiobook, parts, 200_000)?.id)
    }

    @Test
    fun localAndGlobalProgressRoundTripAcrossAPartOffset() {
        val local = audiobookLocalPosition(75_000, 60_000)
        assertEquals(15_000L, local)
        assertEquals(75_000L, audiobookGlobalPosition(local, 60_000))
    }

    @Test
    fun naturalEndAdvancesToTheNextAvailablePart() {
        val withMissingMiddle = parts.toMutableList().also {
            it[1] = it[1].copy(available = false)
        }
        assertEquals(442L, nextAudiobookPartId(withMissingMiddle, 440))
        assertNull(nextAudiobookPartId(withMissingMiddle, 442))
    }
}
