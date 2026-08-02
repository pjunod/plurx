package tv.plurx.app.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.data.Rollup
import tv.plurx.app.data.Watch

class SeriesPlaybackTest {
    @Test
    fun partiallyWatchedEpisodeWinsBeforeTheFirstUnstartedEpisode() {
        val episodes = listOf(
            episode(1),
            episode(2, positionMs = 90_000),
            episode(3, watched = true),
        )

        assertEquals(listOf(2L, 1L, 3L), orderedEpisodeCandidates(episodes).map { it.id })
    }

    @Test
    fun firstUnwatchedEpisodeWinsWhenNothingIsInProgress() {
        val episodes = listOf(episode(1, watched = true), episode(2), episode(3))

        assertEquals(listOf(2L, 3L, 1L), orderedEpisodeCandidates(episodes).map { it.id })
    }

    @Test
    fun inProgressSeasonWinsBeforeUnstartedAndCompletedSeasons() {
        val seasons = listOf(
            season(1, watched = 0, leaves = 12),
            season(2, watched = 3, leaves = 12),
            season(3, watched = 12, leaves = 12),
        )

        assertEquals(listOf(2L, 1L, 3L), orderedSeasonCandidates(seasons).map { it.id })
    }

    @Test
    fun resumePositionRejectsTinyAndNearlyFinishedProgress() {
        assertEquals(0L, resumableStartMs(3_000, 100_000))
        assertEquals(50_000L, resumableStartMs(50_000, 100_000))
        assertEquals(0L, resumableStartMs(96_000, 100_000))
    }

    @Test
    fun aShowWithEpisodesButNoSeasonNodesStillHasAPlayablePath() {
        val episodes = listOf(episode(1), episode(2, positionMs = 90_000))

        assertEquals(listOf(2L, 1L), directShowEpisodeCandidates(episodes).map { it.id })
        assertEquals(emptyList<Item>(), directShowEpisodeCandidates(listOf(season(1, watched = 0, leaves = 8))))
    }

    private fun episode(id: Long, positionMs: Long = 0L, watched: Boolean = false) = Item(
        id = id,
        kind = "episode",
        title = "Episode $id",
        watch = Watch(position_ms = positionMs, watched = watched),
    )

    private fun season(id: Long, watched: Long, leaves: Long) = Item(
        id = id,
        kind = "season",
        title = "Season $id",
        rollup = Rollup(leaves = leaves, watched = watched),
    )
}
