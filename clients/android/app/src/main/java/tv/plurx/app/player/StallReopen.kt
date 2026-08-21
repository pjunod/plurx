package tv.plurx.app.player

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.HlsStart

/** Client-owned bound for repeated stall reopens at one resolved rung. */
internal class StallReopenBudget(private val maxNonDowngrades: Int = 3) {
    var predecessorHeight: Int? = null
    /** Number of times [reset] has been called. Cumulative, never decremented,
     *  used by [Controller] for user-action budget resets and guarded by tests. */
    var resetCount: Int = 0
        private set
    var nonDowngradeCount: Int = 0
        private set
    /**
     * Monotonically increasing sequence advanced by every user-initiated
     * restart (VOD seek, subtitle/audio switch, quality change). Each
     * increment invalidates any in-flight stall reopen from the prior
     * cycle. Read by [Controller] for its [isCurrent] guard and by tests
     * that assert production-linked invalidation.
     */
    var userActionSequence: Long = 0L
        private set

    fun canReopen(): Boolean = nonDowngradeCount < maxNonDowngrades

    fun seed(height: Int?) {
        predecessorHeight = height
    }

    fun record(resolvedHeight: Int?) {
        nonDowngradeCount = if (isStrictDowngrade(predecessorHeight, resolvedHeight)) {
            0
        } else {
            nonDowngradeCount + 1
        }
        predecessorHeight = resolvedHeight
    }

    fun reset() {
        predecessorHeight = null
        nonDowngradeCount = 0
        resetCount++
    }

    /**
     * Production seam that both [Controller] and tests use to advance the
     * user-action sequence and reset the stall budget atomically.  Every
     * user-initiated playback restart (VOD seek, subtitle/audio switch,
     * quality change, leaveSessionPlayback) goes through this method so
     * reverting any one of those Controller paths changes the returned
     * sequence number.
     */
    fun resetForUserAction(): Long {
        reset()
        return ++userActionSequence
    }

    companion object {
        fun isStrictDowngrade(predecessorHeight: Int?, resolvedHeight: Int?): Boolean =
            resolvedHeight != null && resolvedHeight > 0 &&
                predecessorHeight != null && resolvedHeight < predecessorHeight
    }
}

/**
 * Owns the controller's monotonically increasing session-request token.
 *
 * The budget sequence is deliberately not the request token: session opens and
 * stall reopens also consume tokens, so copying the smaller budget sequence can
 * make an old token current again.  The path-specific helpers keep the VOD-seek
 * and in-place subtitle actions directly regression-testable.
 */
internal class ControllerStallGuard(
    private val budget: StallReopenBudget,
) {
    private var requestVersion = 0L

    fun beginRequest(): Long = ++requestVersion

    fun isCurrent(version: Long): Boolean = version == requestVersion

    fun invalidateForUserAction() {
        budget.resetForUserAction()
        requestVersion++
    }

    fun vodSeek(action: () -> Unit) {
        invalidateForUserAction()
        action()
    }

    fun liveSessionSeek(action: () -> Unit) {
        invalidateForUserAction()
        action()
    }

    fun inPlaceSubtitleChange(action: () -> Unit) {
        invalidateForUserAction()
        action()
    }
}

/**
 * Serializes every server-mutating create for one playback id. In particular,
 * a user restart that invalidates a stall while its 400 fallback is in flight
 * is sent after that fallback, so the newer user action remains the server's
 * final replacement as well as the controller's final adopted response.
 *
 * Note: [Mutex] is not reentrant. Internal helpers that already hold the lock
 * must call [callCreate] directly rather than going through [create].
 */
internal class SessionCreateCoordinator(
    private val createSession: suspend (CreateSessionReq) -> HlsStart,
    private val isBadRequest: (Throwable) -> Boolean,
    private val freshRequestId: () -> String,
) {
    private val createMutex = Mutex()

    suspend fun create(
        body: CreateSessionReq,
        isCurrent: () -> Boolean = { true },
    ): HlsStart? = createMutex.withLock {
        if (isCurrent()) callCreate(body) else null
    }

    /**
     * Invoke the raw [create] lambda without re-entering [createMutex].
     * Must only be called from inside [createMutex.withLock].
     */
    private suspend fun callCreate(body: CreateSessionReq): HlsStart {
        try {
            return createSession(body)
        } catch (cancelled: CancellationException) {
            throw cancelled
        }
    }

    suspend fun reopenAfterStall(
        body: CreateSessionReq,
        isCurrent: () -> Boolean = { true },
    ): HlsStart? =
        createMutex.withLock {
            if (!isCurrent()) return@withLock null
            try {
                callCreate(body).takeIf { isCurrent() }
            } catch (failure: Throwable) {
                if (!isBadRequest(failure)) throw failure
                if (!isCurrent()) return@withLock null
                callCreate(
                    body.copy(
                        request_id = freshRequestId(),
                        previous_session_id = null,
                        reopen_reason = null,
                    ),
                ).takeIf { isCurrent() }
            }
        }
}
