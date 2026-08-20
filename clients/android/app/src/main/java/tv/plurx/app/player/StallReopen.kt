package tv.plurx.app.player

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.HlsStart

/** Client-owned bound for repeated stall reopens at one resolved rung. */
internal class StallReopenBudget(private val maxNonDowngrades: Int = 3) {
    var predecessorHeight: Int? = null
        private set
    var nonDowngradeCount: Int = 0
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
    }

    companion object {
        fun isStrictDowngrade(predecessorHeight: Int?, resolvedHeight: Int?): Boolean =
            resolvedHeight != null && resolvedHeight > 0 &&
                predecessorHeight != null && resolvedHeight < predecessorHeight
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
    private val create: suspend (CreateSessionReq) -> HlsStart,
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
            return create(body)
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
                callCreate(body)
            } catch (failure: Throwable) {
                if (!isBadRequest(failure)) throw failure
                if (!isCurrent()) return@withLock null
                callCreate(
                    body.copy(
                        request_id = freshRequestId(),
                        previous_session_id = null,
                        reopen_reason = null,
                    ),
                )
            }
        }
}
