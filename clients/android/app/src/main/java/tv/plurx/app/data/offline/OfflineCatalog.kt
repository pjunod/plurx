package tv.plurx.app.data.offline

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json
import java.io.File
import java.io.FileOutputStream

@Serializable
data class OfflineRecord(
    val id: String,
    val requestId: String,
    val serverInstanceId: String,
    val userId: Long,
    val itemId: Long,
    val fileId: Long,
    val title: String,
    val context: String? = null,
    val posterFile: String? = null,
    val durationMs: Long? = null,
    val requestedHeight: Int,
    val actualHeight: Int? = null,
    val audioIndex: Long? = null,
    val subtitleIndex: Long? = null,
    val audioLabel: String? = null,
    val subtitleLabel: String? = null,
    val packageId: String? = null,
    val leaseToken: String? = null,
    val manifestUrl: String? = null,
    val state: String = "intent",
    val phase: String = "queued",
    val bytesDownloaded: Long = 0,
    val bytesTotal: Long? = null,
    val percentDownloaded: Float = -1f,
    val positionMs: Long = 0,
    val progressRecordedAt: Long? = null,
    val pendingProgress: Boolean = false,
    val errorMessage: String? = null,
    /** Orders Media3 callbacks that can finish catalog IO out of order. */
    val transferSequence: Long = 0,
    val updatedAt: Long = System.currentTimeMillis(),
) {
    val isPlayable: Boolean get() = state == "completed"
}

/** App-private metadata beside Media3's authoritative transfer index. */
class OfflineCatalog(context: Context) {
    private val directory = File(context.filesDir, "offline").apply { mkdirs() }
    private val file = File(directory, "catalog.json")
    private val backup = File(directory, "catalog.backup.json")
    private val mutex = Mutex()
    private val json = Json { ignoreUnknownKeys = true; encodeDefaults = true }
    private val serializer = ListSerializer(OfflineRecord.serializer())
    private val _records = MutableStateFlow(readInitial())

    val records: StateFlow<List<OfflineRecord>> = _records.asStateFlow()

    suspend fun upsert(record: OfflineRecord) = mutate { records ->
        records.filterNot { it.id == record.id } + record.copy(updatedAt = System.currentTimeMillis())
    }

    suspend fun update(id: String, change: (OfflineRecord) -> OfflineRecord) = mutate { records ->
        records.map { if (it.id == id) change(it).copy(updatedAt = System.currentTimeMillis()) else it }
    }

    suspend fun remove(id: String): OfflineRecord? {
        var removed: OfflineRecord? = null
        mutate { records ->
            removed = records.firstOrNull { it.id == id }
            records.filterNot { it.id == id }
        }
        return removed
    }

    fun record(id: String): OfflineRecord? = _records.value.firstOrNull { it.id == id }

    fun profile(serverInstanceId: String?, userId: Long?): List<OfflineRecord> {
        if (serverInstanceId == null || userId == null) return emptyList()
        return _records.value.filter {
            it.serverInstanceId == serverInstanceId && it.userId == userId
        }.sortedByDescending { it.updatedAt }
    }

    suspend fun removeProfile(serverInstanceId: String, userId: Long): List<OfflineRecord> {
        var removed = emptyList<OfflineRecord>()
        mutate { records ->
            removed = records.filter {
                it.serverInstanceId == serverInstanceId && it.userId == userId
            }
            records - removed.toSet()
        }
        return removed
    }

    private suspend fun mutate(change: (List<OfflineRecord>) -> List<OfflineRecord>) {
        mutex.withLock {
            val updated = change(_records.value)
            withContext(Dispatchers.IO) { write(updated) }
            _records.value = updated
        }
    }

    private fun readInitial(): List<OfflineRecord> {
        for (candidate in listOf(file, backup)) {
            val decoded = runCatching {
                if (!candidate.exists()) null
                else json.decodeFromString(serializer, candidate.readText())
            }.getOrNull()
            if (decoded != null) return decoded
        }
        return emptyList()
    }

    private fun write(records: List<OfflineRecord>) {
        directory.mkdirs()
        val temporary = File(directory, "catalog.json.tmp")
        FileOutputStream(temporary).use { output ->
            output.write(json.encodeToString(serializer, records).toByteArray())
            output.fd.sync()
        }
        if (file.exists()) file.copyTo(backup, overwrite = true)
        check(temporary.renameTo(file)) { "could not commit offline catalog" }
    }
}
