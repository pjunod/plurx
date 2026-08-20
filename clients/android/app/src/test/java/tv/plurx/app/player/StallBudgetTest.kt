package tv.plurx.app.player

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.HlsStart
import tv.plurx.app.data.ReopenReason

/** Regression guards for the production stall-reopen transition. */
class StallBudgetTest {
    @Test
    fun budgetStopsAfterThreeSameRungAnswers() {
        val budget = StallReopenBudget()
        budget.seed(240)

        repeat(3) {
            assertTrue(budget.canReopen())
            budget.record(240)
        }

        assertFalse(budget.canReopen())
        assertEquals(3, budget.nonDowngradeCount)
    }

    @Test
    fun strictDowngradeResetsBudgetAtThePredecessorsOwnRung() {
        val budget = StallReopenBudget()
        budget.seed(360)
        budget.record(360)
        budget.record(240)

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertEquals(240, budget.predecessorHeight)
    }

    @Test
    fun absentSameOrHigherAnswerConsumesBudget() {
        val budget = StallReopenBudget()
        budget.seed(480)

        budget.record(null)
        budget.seed(480)
        budget.record(480)
        budget.record(720)

        assertFalse(budget.canReopen())
    }

    @Test
    fun resetInvalidatesTheOldFloorBudget() {
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }

        budget.reset()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)
    }

    @Test
    fun badRequestRetriesExactlyOnceUnboundWithFreshRequestId() = runBlocking {
        val calls = mutableListOf<CreateSessionReq>()
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = { body ->
                calls += body
                if (calls.size == 1) throw badRequest
                response("replacement")
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "fresh-request" },
        )

        val result = coordinator.reopenAfterStall(stallBody())

        assertEquals("replacement", result?.session_id)
        assertEquals(2, calls.size)
        assertEquals("predecessor", calls[0].previous_session_id)
        assertEquals(ReopenReason.Stall, calls[0].reopen_reason)
        assertEquals("fresh-request", calls[1].request_id)
        assertNull(calls[1].previous_session_id)
        assertNull(calls[1].reopen_reason)
    }

    @Test
    fun nonBadRequestIsTerminalWithoutFallback() = runBlocking {
        val terminal = IllegalStateException("server failed")
        var calls = 0
        val coordinator = SessionCreateCoordinator(
            createSession = {
                calls++
                throw terminal
            },
            isBadRequest = { false },
            freshRequestId = { "unused" },
        )

        try {
            coordinator.reopenAfterStall(stallBody())
            fail("expected terminal failure")
        } catch (failure: Throwable) {
            assertSame(terminal, failure)
        }
        assertEquals(1, calls)
    }

    @Test
    fun invalidationBeforeFallbackPreventsTheRetry() = runBlocking {
        var current = true
        var calls = 0
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = {
                calls++
                current = false
                throw badRequest
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "unused" },
        )

        assertNull(coordinator.reopenAfterStall(stallBody()) { current })
        assertEquals(1, calls)
    }

    @Test
    fun newerUserCreateIsTheFinalServerMutationAfterInFlightFallback() = runBlocking {
        val calls = mutableListOf<CreateSessionReq>()
        val fallbackStarted = CompletableDeferred<Unit>()
        val finishFallback = CompletableDeferred<Unit>()
        val badRequest = TestBadRequest()
        var stallCurrent = true
        val coordinator = SessionCreateCoordinator(
            createSession = { body ->
                calls += body
                when (body.request_id) {
                    "stall-request" -> throw badRequest
                    "fallback-request" -> {
                        fallbackStarted.complete(Unit)
                        finishFallback.await()
                        response("stale-fallback")
                    }
                    else -> response("user-session")
                }
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "fallback-request" },
        )

        val stall = async {
            coordinator.reopenAfterStall(stallBody()) { stallCurrent }
        }
        fallbackStarted.await()
        stallCurrent = false
        val user = async {
            coordinator.create(
                CreateSessionReq(playback_id = "playback", request_id = "user-request"),
            )
        }
        finishFallback.complete(Unit)

        assertEquals("stale-fallback", stall.await()?.session_id)
        assertEquals("user-session", user.await()?.session_id)
        assertEquals(
            listOf("stall-request", "fallback-request", "user-request"),
            calls.map { it.request_id },
        )
    }


    @Test
    fun vodSeekAfterExhaustionResetsBudget() {
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }
        assertFalse(budget.canReopen())

        // VOD seek resets the budget
        budget.reset()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)

        // After reset, budget counts from zero, not from the exhausted state
        budget.record(240)
        assertEquals(1, budget.nonDowngradeCount)
        assertTrue(budget.canReopen())
    }

    @Test
    fun subtitleTrackChangeResetsBudget() {
        val budget = StallReopenBudget()
        budget.seed(1080)
        budget.record(720)
        assertEquals(1, budget.nonDowngradeCount)
        assertEquals(720, budget.predecessorHeight)

        // Subtitle track change resets the budget
        budget.reset()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)

        // A subsequent reopen on the new track starts fresh
        budget.seed(480)
        assertTrue(budget.canReopen())
    }

    private fun stallBody() = CreateSessionReq(
        playback_id = "playback",
        request_id = "stall-request",
        height = 1080,
        start = 4.0,
        previous_session_id = "predecessor",
        reopen_reason = ReopenReason.Stall,
    )

    private fun response(id: String) = HlsStart(
        session_id = id,
        playlist_url = "/sessions/$id/master.m3u8",
        height = 1080,
    )

    private class TestBadRequest : RuntimeException()
}
