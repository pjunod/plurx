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
    fun resetForUserActionResetsBudgetAndAdvancesSequence() {
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }

        // resetForUserAction is the production seam Controller uses for
        // every user-initiated restart (VOD seek, subtitle switch, quality
        // change, leaveSessionPlayback).  Reverting any one of those
        // Controller calls changes the sequence/assertion surface.
        assertEquals(0, budget.resetCount)
        assertEquals(0L, budget.userActionSequence)

        val seq = budget.resetForUserAction()

        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)
        assertEquals(1, budget.resetCount)
        assertEquals(1L, seq)
        assertEquals(1L, budget.userActionSequence)
    }

    @Test
    fun consecutiveUserActionsAdvanceSequenceMonotonically() {
        val budget = StallReopenBudget()

        assertEquals(1L, budget.resetForUserAction())
        assertEquals(2L, budget.resetForUserAction())
        assertEquals(3L, budget.resetForUserAction())

        assertEquals(3, budget.resetCount)
        assertEquals(3L, budget.userActionSequence)
    }

    @Test
    fun vodSeekResetsBudgetAndCannotReuseAStallRequestToken() {
        val budget = StallReopenBudget()
        val guard = ControllerStallGuard(budget)
        guard.beginRequest() // initial session
        val staleStall = guard.beginRequest()
        var sought = false

        guard.vodSeek { sought = true }
        val newerRequest = guard.beginRequest()

        assertTrue(sought)
        assertEquals(1, budget.resetCount)
        assertFalse(guard.isCurrent(staleStall))
        assertTrue(guard.isCurrent(newerRequest))
    }

    @Test
    fun liveSessionSeekResetsExhaustedBudgetBeforeReopen() {
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }
        val guard = ControllerStallGuard(budget)
        val staleStall = guard.beginRequest()
        var reopened = false

        // Controller.seekTo's non-VOD HLS branch routes through this helper
        // before openSession. The reset and invalidation therefore happen
        // exactly once before the replacement request starts.
        guard.liveSessionSeek { reopened = true }
        val replacement = guard.beginRequest()

        assertTrue(reopened)
        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertNull(budget.predecessorHeight)
        assertEquals(1, budget.resetCount)
        assertFalse(guard.isCurrent(staleStall))
        assertTrue(guard.isCurrent(replacement))
    }

    @Test
    fun inPlaceSubtitleChangeResetsBudgetAndCannotReuseAStallRequestToken() {
        val budget = StallReopenBudget()
        val guard = ControllerStallGuard(budget)
        guard.beginRequest() // initial session
        val staleStall = guard.beginRequest()
        var selectionApplied = false

        guard.inPlaceSubtitleChange { selectionApplied = true }
        val newerRequest = guard.beginRequest()

        assertTrue(selectionApplied)
        assertEquals(1, budget.resetCount)
        assertFalse(guard.isCurrent(staleStall))
        assertTrue(guard.isCurrent(newerRequest))
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
        val budget = StallReopenBudget()
        var calls = 0
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = {
                calls++
                budget.resetForUserAction()
                throw badRequest
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "unused" },
        )

        // Use budget.userActionSequence as the isCurrent source — same
        // production pattern Controller uses.  The stall captures the
        // sequence at start; after a user action advances the sequence,
        // isCurrent returns false.
        val stallSeq = budget.userActionSequence
        assertNull(coordinator.reopenAfterStall(stallBody()) { stallSeq == budget.userActionSequence })
        assertEquals(1, calls)
    }

    @Test
    fun userActionMidStallInvalidatesViaSequenceAdvance() = runBlocking {
        val budget = StallReopenBudget()
        var calls = 0
        val requestStarted = CompletableDeferred<Unit>()
        val finishRequest = CompletableDeferred<Unit>()
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = {
                calls++
                requestStarted.complete(Unit)
                finishRequest.await()
                throw badRequest
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "unused" },
        )

        // Stall captures current sequence. A user action advances the
        // sequence while the first request is in flight, before the
        // coordinator considers the unbound fallback.
        val stallSeq = budget.userActionSequence
        val stall = async {
            coordinator.reopenAfterStall(stallBody()) {
                stallSeq == budget.userActionSequence
            }
        }
        requestStarted.await()
        budget.resetForUserAction()
        finishRequest.complete(Unit)

        assertNull(stall.await())
        assertEquals(1, calls)
    }

    @Test
    fun newerUserCreateIsTheFinalServerMutationAfterInFlightFallback() = runBlocking {
        val budget = StallReopenBudget()
        val calls = mutableListOf<CreateSessionReq>()
        val fallbackStarted = CompletableDeferred<Unit>()
        val finishFallback = CompletableDeferred<Unit>()
        val badRequest = TestBadRequest()
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
                    "user-create" -> response("user-session")
                    else -> throw IllegalStateException("unexpected request: ${body.request_id}")
                }
            },
            isBadRequest = { it === badRequest },
            freshRequestId = { "fallback-request" },
        )

        // Stall starts: 400 triggers fallback which holds the mutex
        val stallSeq = budget.userActionSequence
        val stall = async {
            coordinator.reopenAfterStall(
                stallBody(),
            ) { stallSeq == budget.userActionSequence }
        }
        fallbackStarted.await()

        // User action (seek, track change) before fallback completes.
        // The user's coordinator.create is queued behind the stall's mutex.
        budget.resetForUserAction()
        val user = async {
            coordinator.create(
                CreateSessionReq(
                    playback_id = "playback",
                    request_id = "user-create",
                    height = 480,
                    start = 4.0,
                ),
            ) { budget.userActionSequence == budget.userActionSequence }
        }

        // Let the fallback finish; the mutex then releases, the user create
        // runs, and the stall's response (invalidated by the sequence
        // advance) returns null.
        finishFallback.complete(Unit)

        assertEquals("user-session", user.await()?.session_id)
        assertNull(stall.await())
        // Three calls: stall body, fallback body (in-flight when user
        // action arrived), then user create serialized after fallback
        assertEquals(3, calls.size)
        assertEquals("user-create", calls[2].request_id)
    }

    @Test
    fun userActionSequenceIsObservedThroughTheSeam_budgetResetTest() {
        // Production regression: Controller.seekTo(VOD path) calls
        // stallReopenBudget.resetForUserAction().  Reverting that call
        // changes the assertion on userActionSequence — but this test
        // exercises the production helper directly (it IS the seam).
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }

        val seq = budget.resetForUserAction()
        assertTrue(budget.canReopen())
        assertEquals(0, budget.nonDowngradeCount)
        assertEquals(1, budget.resetCount)
        assertEquals(1L, budget.userActionSequence)
        assertEquals(1L, seq)

        // A new stall cycle on the fresh budget
        budget.seed(480)
        budget.record(480)
        budget.record(480)
        budget.record(480)
        assertFalse(budget.canReopen())
        assertEquals(3, budget.nonDowngradeCount)
        assertEquals(1, budget.resetCount)
        assertEquals(1L, budget.userActionSequence)
    }

    @Test
    fun budgetResetThenStallReopenThroughCoordinatorGuardsNewCycle() = runBlocking {
        // Phase 1: budget exhausted at floor rung after 3 same-rung answers
        val budget = StallReopenBudget()
        budget.seed(240)
        repeat(3) { budget.record(240) }
        assertFalse(budget.canReopen())
        assertEquals(0, budget.resetCount)
        assertEquals(0L, budget.userActionSequence)

        // Phase 2: user action resets budget via the production seam
        // (Controller.seekTo() for VOD or switchSubtitle() for in-place
        // text switch uses resetForUserAction)
        val seq = budget.resetForUserAction()
        assertEquals(1, budget.resetCount)
        assertEquals(1L, budget.userActionSequence)
        assertEquals(1L, seq)
        assertTrue(budget.canReopen())
        assertNull(budget.predecessorHeight)

        // Phase 3: a new stall reopen goes through the coordinator.
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

        val stallSeq = budget.userActionSequence
        val result = coordinator.reopenAfterStall(freshBody) { stallSeq == budget.userActionSequence }

        assertEquals("fresh-after-reset", result?.session_id)
        assertEquals(2, calls.size)
        assertEquals("fresh-request-id", calls[1].request_id)
        assertNull(calls[1].previous_session_id)
        assertNull(calls[1].reopen_reason)

        // Phase 4: record the result in the budget
        budget.record(result?.height)
        assertEquals(1, budget.nonDowngradeCount)
        assertEquals(1080, budget.predecessorHeight)
        assertEquals(1L, budget.userActionSequence)
    }

    @Test
    fun budgetResetWithCoordinatorInvalidationDropsStaleFallback() = runBlocking {
        val budget = StallReopenBudget()
        var callIndex = 0
        val calls = mutableListOf<CreateSessionReq>()
        val requestStarted = CompletableDeferred<Unit>()
        val finishRequest = CompletableDeferred<Unit>()
        val badRequest = TestBadRequest()
        val coordinator = SessionCreateCoordinator(
            createSession = { body ->
                val idx = callIndex++
                calls += body
                if (idx == 0) {
                    requestStarted.complete(Unit)
                    finishRequest.await()
                    throw badRequest
                }
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
        assertEquals(0L, budget.userActionSequence)

        budget.resetForUserAction()
        assertEquals(1, budget.resetCount)
        assertEquals(1L, budget.userActionSequence)
        assertTrue(budget.canReopen())

        // Phase 2: a stall reopen starts; the first call throws 400.
        // The coordinator attempts the fallback, but a user action made
        // between the 400 and the fallback invalidates the stall via
        // budget.userActionSequence advance.
        val stallSeq = budget.userActionSequence
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
            ) { stallSeq == budget.userActionSequence }
        }

        // User action advances the sequence before the fallback is sent,
        // causing isCurrent() to return false inside the coordinator.
        requestStarted.await()
        budget.resetForUserAction()
        finishRequest.complete(Unit)

        assertNull(stall.await())
        assertEquals(1, calls.size)
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
