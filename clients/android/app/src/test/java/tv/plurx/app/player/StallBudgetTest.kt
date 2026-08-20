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
        assertEquals(0, budget.resetCount)
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
        assertEquals(0, budget.resetCount)
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

        assertEquals(0, budget.resetCount)
        budget.reset()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)
        assertEquals(1, budget.resetCount)
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
        assertEquals(0, budget.resetCount)

        // VOD seek resets the budget (as Controller.seekTo() does for sessionIsVod)
        budget.reset()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)
        assertEquals(1, budget.resetCount)

        // After reset, budget counts from zero, not from the exhausted state
        budget.record(240)
        assertEquals(1, budget.nonDowngradeCount)
        assertTrue(budget.canReopen())
    }

    @Test
    fun subtitleTrackChangeResetsBudget() {
        val budget = StallReopenBudget()
        budget.seed(1080)
        budget.record(1080)
        assertEquals(1, budget.nonDowngradeCount)
        assertEquals(1080, budget.predecessorHeight)
        assertEquals(0, budget.resetCount)

        // Subtitle track change resets the budget
        // (as Controller.switchSubtitle() does for in-place text-to-text switches)
        budget.reset()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)
        assertEquals(1, budget.resetCount)

        // A subsequent reopen on the new track starts fresh
        budget.seed(480)
        assertTrue(budget.canReopen())
    }

    @Test
    fun budgetResetThenStallReopenThroughCoordinatorGuardsNewCycle() = runBlocking {
        // Phase 1: budget exhausted at floor rung after 3 same-rung answers
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }
        assertFalse(budget.canReopen())
        assertEquals(0, budget.resetCount)

        // Phase 2: user seek/subtitle-change resets the budget
        // (Controller.seekTo() for VOD or switchSubtitle() for in-place text switch)
        budget.reset()
        assertEquals(1, budget.resetCount)
        assertTrue(budget.canReopen())
        assertNull(budget.predecessorHeight)

        // Phase 3: a new stall reopen goes through the coordinator.
        // The stall body gets a 400, the fallback succeeds.
        var calls = mutableListOf<CreateSessionReq>()
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = { body ->
                calls += body
                if (calls.size == 1) throw badRequest
                response("fresh-after-reset")
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "fresh-request-id" },
        )

        val freshBody = CreateSessionReq(
            playback_id = "playback",
            request_id = "new-stall",
            height = 240,
            start = 4.0,
            previous_session_id = "session-after-seek",
            reopen_reason = ReopenReason.Stall,
        )

        val result = coordinator.reopenAfterStall(freshBody)

        assertEquals("fresh-after-reset", result?.session_id)
        assertEquals(2, calls.size)
        assertEquals("fresh-request-id", calls[1].request_id)
        assertNull(calls[1].previous_session_id) // unbound fallback
        assertNull(calls[1].reopen_reason)

        // Phase 4: record the result in the budget — starts counting fresh
        // from the new cycle's first rung
        budget.record(result?.height)
        assertEquals(1, budget.nonDowngradeCount)
        assertEquals(1080, budget.predecessorHeight)
    }

    @Test
    fun budgetResetWithCoordinatorInvalidationDropsStaleFallback() = runBlocking {
        val budget = StallReopenBudget()
        var stallCurrent = true
        var callIndex = 0
        val calls = mutableListOf<CreateSessionReq>()
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = { body ->
                val idx = callIndex++
                calls += body
                if (idx == 0) throw badRequest
                response("fallback")
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "fallback-req" },
        )

        // Phase 1: budget exhausted, then user action resets
        budget.seed(240)
        repeat(3) { budget.record(240) }
        assertFalse(budget.canReopen())
        assertEquals(0, budget.resetCount)

        budget.reset()
        assertEquals(1, budget.resetCount)
        assertTrue(budget.canReopen())

        // Phase 2: a stall reopen starts; the first call (stall body)
        // throws 400. The coordinator attempts the fallback, but
        // a user action made between the 400 and the fallback invalidates
        // the stall — isCurrent() returns false before the fallback is sent.
        val stall = async {
            coordinator.reopenAfterStall(
                CreateSessionReq(
                    playback_id = "playback",
                    request_id = "stall-req",
                    height = 240,
                    start = 4.0,
                    previous_session_id = "prev",
                    reopen_reason = ReopenReason.Stall,
                ),
            ) { stallCurrent }
        }
        // User action invalidates the stall before the fallback is sent.
        // The coordinator checks isCurrent() after catching the 400
        // and before calling the fallback; returning false skips it.
        stallCurrent = false

        assertEquals(1, calls.size) // only the stall body; fallback skipped
        assertNull(stall.await())
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
