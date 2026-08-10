package tv.plurx.app.data.offline

import android.content.Context
import tv.plurx.app.data.OfflineNetwork

/**
 * The small, synchronous half of offline recovery.
 *
 * The catalog remains the display record, and Media3 remains authoritative for
 * downloaded bytes. These preferences exist for the state that Android must be
 * able to read before either asynchronous owner is ready after a process start:
 * the network constraint, the durable local intent, and whether the system has
 * stopped a transfer until the viewer explicitly resumes it.
 */
internal class OfflineRecoveryStore(context: Context) {
    private val preferences = context.getSharedPreferences(NAME, Context.MODE_PRIVATE)

    fun migrateNetworkPolicy(legacy: OfflineNetwork): OfflineNetwork {
        if (!preferences.contains(NETWORK)) {
            check(preferences.edit().putString(NETWORK, legacy.storageValue).commit()) {
                "could not migrate offline network policy"
            }
        }
        return networkPolicy()
    }

    fun networkPolicy(): OfflineNetwork =
        OfflineNetwork.fromStorage(preferences.getString(NETWORK, null))

    fun setNetworkPolicy(policy: OfflineNetwork) {
        check(preferences.edit().putString(NETWORK, policy.storageValue).commit()) {
            "could not persist offline network policy"
        }
    }

    fun persistIntent(id: String, encodedRecord: String) {
        check(preferences.edit().putString(INTENT_PREFIX + id, encodedRecord).commit()) {
            "could not persist offline download intent"
        }
    }

    fun intents(): Map<String, String> = preferences.all.mapNotNull { (key, value) ->
        if (key.startsWith(INTENT_PREFIX) && value is String) {
            key.removePrefix(INTENT_PREFIX) to value
        } else {
            null
        }
    }.toMap()

    fun clearIntent(id: String) {
        check(preferences.edit().remove(INTENT_PREFIX + id).commit()) {
            "could not retire offline download intent"
        }
    }

    fun setTransferState(id: String, state: TransferRecoveryState) {
        check(preferences.edit().putString(TRANSFER_PREFIX + id, state.storageValue).commit()) {
            "could not persist offline transfer state"
        }
    }

    fun transferState(id: String): TransferRecoveryState =
        TransferRecoveryState.fromStorage(preferences.getString(TRANSFER_PREFIX + id, null))

    fun removeTransfer(id: String) {
        check(
            preferences.edit()
                .remove(TRANSFER_PREFIX + id)
                .remove(JOB_PREFIX + id)
                .commit(),
        ) {
            "could not retire offline transfer state"
        }
    }

    @Synchronized
    fun jobId(id: String): Int {
        val key = JOB_PREFIX + id
        val existing = preferences.getInt(key, 0)
        if (existing != 0) return existing
        val allocated = preferences.getInt(NEXT_JOB_ID, FIRST_JOB_ID)
        val next = if (allocated == Int.MAX_VALUE) FIRST_JOB_ID else allocated + 1
        check(preferences.edit().putInt(key, allocated).putInt(NEXT_JOB_ID, next).commit()) {
            "could not allocate offline transfer job"
        }
        return allocated
    }

    fun existingJobId(id: String): Int? = preferences.getInt(JOB_PREFIX + id, 0)
        .takeIf { it != 0 }

    private companion object {
        const val NAME = "offline-recovery-v25"
        const val NETWORK = "network"
        const val INTENT_PREFIX = "intent."
        const val TRANSFER_PREFIX = "transfer."
        const val JOB_PREFIX = "job."
        const val NEXT_JOB_ID = "next_job_id"
        const val FIRST_JOB_ID = 20_400
    }
}

internal enum class TransferRecoveryState(val storageValue: String) {
    Preparing("preparing"),
    WaitingForJob("waiting_for_job"),
    Active("active"),
    PausedBySystem("paused_by_system"),
    Completed("completed"),
    ;

    companion object {
        fun fromStorage(value: String?): TransferRecoveryState = entries.firstOrNull {
            it.storageValue == value
        } ?: Preparing
    }
}
