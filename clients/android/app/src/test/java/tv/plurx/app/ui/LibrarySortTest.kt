package tv.plurx.app.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.data.Rollup
import tv.plurx.app.data.Watch

private fun movie(id: Long, title: String, addedAt: Long?) =
    Item(id = id, kind = "movie", title = title, added_at = addedAt)

private fun show(id: Long, leaves: Long, watched: Long) =
    Item(id = id, kind = "show", title = "Show $id", rollup = Rollup(leaves = leaves, watched = watched))

class LibrarySortTest {

    @Test
    fun addedSortInterleavesSeveralSharesByRecency() {
        // Two shares, each already sorted by the server. Concatenated they
        // are two runs laid end to end; "Recently added" has to interleave.
        val merged = listOf(
            movie(1, "Share A newer", 300),
            movie(2, "Share A older", 100),
            movie(3, "Share B newest", 400),
            movie(4, "Share B middle", 200),
        )

        assertEquals(listOf(3L, 1L, 4L, 2L), sortMerged(merged, "added").map { it.id })
    }

    @Test
    fun showsFilterOnTheirRollupBecauseAContainerHasNoWatchRow() {
        // The bug this fixes: a TV library filtered on `item.watch` alone, and
        // a show has none — so every show landed in "Unwatched", including the
        // one you finished last night.
        val finished = show(1, leaves = 20, watched = 20)
        val halfway = show(2, leaves = 20, watched = 9)
        val untouched = show(3, leaves = 20, watched = 0)
        val library = listOf(finished, halfway, untouched)

        assertEquals(listOf(1L), library.filter { matchesFilter(it, WatchFilter.Watched) }.map { it.id })
        assertEquals(listOf(2L), library.filter { matchesFilter(it, WatchFilter.InProgress) }.map { it.id })
        assertEquals(listOf(3L), library.filter { matchesFilter(it, WatchFilter.Unwatched) }.map { it.id })
        assertEquals(3, library.count { matchesFilter(it, WatchFilter.Everything) })
    }

    @Test
    fun leavesAndOlderServersKeepTheWatchRowBehaviour() {
        val watched = Item(id = 4, kind = "movie", title = "Seen", watch = Watch(watched = true))
        val started = Item(id = 5, kind = "movie", title = "Started", watch = Watch(position_ms = 90_000))
        val fresh = Item(id = 6, kind = "movie", title = "Fresh")
        // A container from a server that predates the batched rollup: no
        // rollup, so it keeps the old answer rather than a new wrong one.
        val preRollupShow = Item(id = 7, kind = "show", title = "Old server")
        val library = listOf(watched, started, fresh, preRollupShow)

        assertEquals(listOf(4L), library.filter { matchesFilter(it, WatchFilter.Watched) }.map { it.id })
        assertEquals(listOf(5L), library.filter { matchesFilter(it, WatchFilter.InProgress) }.map { it.id })
        assertEquals(listOf(6L, 7L), library.filter { matchesFilter(it, WatchFilter.Unwatched) }.map { it.id })
    }

    @Test
    fun aContainerWithNoLeavesIsNotSecretlyWatched() {
        // `watched >= leaves` is true for 0 >= 0. An empty season is unwatched.
        val empty = show(8, leaves = 0, watched = 0)
        assertEquals(false, matchesFilter(empty, WatchFilter.Watched))
        assertEquals(true, matchesFilter(empty, WatchFilter.Unwatched))
    }

    @Test
    fun itemsWithNoAddedAtSinkAndTieBreakOnTitle() {
        val merged = listOf(
            movie(1, "Zulu", null),
            movie(2, "Alpha", null),
            movie(3, "Dated", 10),
        )
        assertEquals(listOf(3L, 2L, 1L), sortMerged(merged, "added").map { it.id })
    }
}
