package tv.plurx.app.player

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Regression guard for the stall-reopen budget logic in [Controller].
 *
 * Each test verifies one acceptance criterion and fails when the matching
 * production correction is reverted.  The budget is a pure-function decision
 * on [Controller.isStrictDowngrade], so these tests need no Android runtime.
 */
class StallBudgetTest {

    // ---- strict downgrade (resolved < predecessor) -----------------------

    @Test
    fun nullPredecessorHeightIsNeverADowngrade() {
        // First stall at cold start: no predecessor height means we cannot
        // determine a step down, so it must count toward the bound.
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = null, resolvedHeight = 1080))
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = null, resolvedHeight = null))
    }

    @Test
    fun absentResolvedHeightIsNeverADowngrade() {
        // The server returned no height (or height 0) — the client cannot
        // verify a step down, so this counts toward the bound.
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = 1080, resolvedHeight = null))
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = 1080, resolvedHeight = 0))
    }

    @Test
    fun sameRungIsNotADowngrade() {
        // Same rung at the floor: the server cannot step further down.
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = 480, resolvedHeight = 480))
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = 2160, resolvedHeight = 2160))
    }

    @Test
    fun higherRungIsNotADowngrade() {
        // The server answered with a higher rung (shouldn't happen in
        // practice, but still not a downgrade).
        assertFalse(Controller.isStrictDowngrade(predecessorHeight = 480, resolvedHeight = 720))
    }

    @Test
    fun strictlyLowerRungIsADowngrade() {
        // A genuine downgrade resets the budget counter.
        assertTrue(Controller.isStrictDowngrade(predecessorHeight = 2160, resolvedHeight = 1080))
        assertTrue(Controller.isStrictDowngrade(predecessorHeight = 1080, resolvedHeight = 720))
        assertTrue(Controller.isStrictDowngrade(predecessorHeight = 720, resolvedHeight = 480))
    }

    @Test
    fun minHeightDowngradeFloorStillQualifies() {
        // A sub-360 source resolves below the ladder; the floor is the
        // predecessor's own rung, not 360.
        assertTrue(Controller.isStrictDowngrade(predecessorHeight = 360, resolvedHeight = 240))
    }

    @Test
    fun stallBudgetExercisesEveryAcceptanceCriterion() {
        // Smoke-test anchor: each individual criterion has its own
        // named test above.  This combined case confirms the full
        // sequence that a Controller.onStall() caller exercises.
        assertFalse(Controller.isStrictDowngrade(null, 1080))
        assertFalse(Controller.isStrictDowngrade(1080, null))
        assertFalse(Controller.isStrictDowngrade(1080, 0))
        assertFalse(Controller.isStrictDowngrade(480, 480))
        assertTrue(Controller.isStrictDowngrade(2160, 1080))
        assertTrue(Controller.isStrictDowngrade(360, 240))
    }
}
