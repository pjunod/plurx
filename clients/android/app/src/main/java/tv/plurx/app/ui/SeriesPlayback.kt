package tv.plurx.app.ui

import tv.plurx.app.data.Item

internal fun orderedEpisodeCandidates(items: List<Item>): List<Item> {
    val episodes = items.filter { it.kind == "episode" }
    val inProgress = episodes.filter {
        it.watch?.watched != true && (it.watch?.position_ms ?: 0L) > 3_000L
    }
    val unwatched = episodes.filter {
        it.watch?.watched != true && (it.watch?.position_ms ?: 0L) <= 3_000L
    }
    val watched = episodes.filter { it.watch?.watched == true }
    return inProgress + unwatched + watched
}

internal fun orderedSeasonCandidates(items: List<Item>): List<Item> {
    val seasons = items.filter { it.kind == "season" }
    val inProgress = seasons.filter {
        val rollup = it.rollup
        rollup != null && rollup.leaves > 0 && rollup.watched in 1 until rollup.leaves
    }
    val notStarted = seasons.filter { (it.rollup?.watched ?: 0L) == 0L }
    val completed = seasons.filter { it !in inProgress && it !in notStarted }
    return inProgress + notStarted + completed
}

internal fun resumableStartMs(positionMs: Long, durationMs: Long?): Long {
    if (positionMs <= 3_000L) return 0L
    if (durationMs != null && durationMs > 0L && positionMs.toDouble() > durationMs * 0.95) return 0L
    return positionMs
}
