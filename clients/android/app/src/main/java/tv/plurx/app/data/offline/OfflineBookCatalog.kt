package tv.plurx.app.data.offline

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
import tv.plurx.app.data.PublicationLimits
import tv.plurx.app.data.PublicationManifest
import tv.plurx.app.data.ReadingLocator
import tv.plurx.app.data.ReadingRevision
import java.io.File
import java.io.FileOutputStream

@Serializable
data class OfflineBookPreferences(
    val font: String = "publisher",
    val fontSize: Double = 100.0,
    val lineHeight: Double = 1.55,
    val margin: Double = 7.0,
    val theme: String = "light",
    val flow: String = "paginated",
)

@Serializable
data class OfflineBook(
    val id: String,
    val serverInstanceId: String,
    val userId: Long,
    val itemId: Long,
    val fileId: Long,
    val revision: ReadingRevision? = null,
    val title: String,
    val author: String? = null,
    val originalFilename: String = "book.epub",
    val coverFilename: String? = null,
    val publication: PublicationManifest? = null,
    val limits: PublicationLimits? = null,
    val state: String = "intent",
    val phase: String = "opening_publication",
    val bytesDownloaded: Long = 0,
    val bytesTotal: Long = 0,
    val locator: ReadingLocator? = null,
    val progression: Double = 0.0,
    val completed: Boolean = false,
    val recordedAt: Long? = null,
    val pendingProgress: Boolean = false,
    val preferences: OfflineBookPreferences = OfflineBookPreferences(),
    val errorMessage: String? = null,
    val updatedAt: Long = System.currentTimeMillis(),
) {
    val isPlayable: Boolean
        get() = state == "downloaded" && revision != null && publication != null && limits != null
}

/** Normalize one manifest href without ever producing an absolute path. */
internal fun safePublicationPath(href: String): String? {
    val rawPath = href.substringBefore('#')
    val output = mutableListOf<String>()
    for (raw in rawPath.split('/')) {
        if (raw.isEmpty() || raw == ".") continue
        // URLDecoder implements HTML form semantics and would turn a literal
        // `+` in an EPUB filename into a space. Escape it first so this is
        // percent-decoding only; capability paths and local paths must name
        // exactly the same publication entry.
        val part = runCatching {
            java.net.URLDecoder.decode(raw.replace("+", "%2B"), Charsets.UTF_8.name())
        }
            .getOrElse { raw }
        when {
            part == ".." -> if (output.isEmpty()) return null else output.removeAt(output.lastIndex)
            part.isEmpty() || '/' in part || '\\' in part || ':' in part ||
                part.any { it.code < 0x20 || it.code == 0x7f } -> return null
            else -> output += part
        }
    }
    return output.takeIf { it.isNotEmpty() }?.joinToString("/")
}

/** Durable profile catalogue beside, but separate from, Media3 video state. */
class OfflineBookCatalog(private val directory: File) {
    private val index = File(directory, "catalog.json")
    private val backup = File(directory, "catalog.backup.json")
    private val mutex = Mutex()
    private val json = Json { ignoreUnknownKeys = true; encodeDefaults = true }
    private val serializer = ListSerializer(OfflineBook.serializer())
    private val _records: MutableStateFlow<List<OfflineBook>>

    init {
        directory.mkdirs()
        _records = MutableStateFlow(readInitial())
    }

    val records: StateFlow<List<OfflineBook>> = _records.asStateFlow()

    fun record(id: String): OfflineBook? = _records.value.firstOrNull { it.id == id }

    fun profile(serverInstanceId: String?, userId: Long?): List<OfflineBook> {
        if (serverInstanceId == null || userId == null) return emptyList()
        return _records.value.filter {
            it.serverInstanceId == serverInstanceId && it.userId == userId
        }.sortedByDescending(OfflineBook::updatedAt)
    }

    suspend fun upsert(book: OfflineBook) = mutate { books ->
        books.filterNot { it.id == book.id } + book.copy(updatedAt = System.currentTimeMillis())
    }

    suspend fun update(id: String, change: (OfflineBook) -> OfflineBook): OfflineBook? {
        var updated: OfflineBook? = null
        mutate { books ->
            books.map { book ->
                if (book.id != id) book else change(book).copy(
                    updatedAt = System.currentTimeMillis(),
                ).also { updated = it }
            }
        }
        return updated
    }

    suspend fun remove(id: String): OfflineBook? {
        var removed: OfflineBook? = null
        mutate { books ->
            removed = books.firstOrNull { it.id == id }
            books.filterNot { it.id == id }
        }
        return removed
    }

    suspend fun removeProfile(serverInstanceId: String, userId: Long): List<OfflineBook> {
        var removed = emptyList<OfflineBook>()
        mutate { books ->
            removed = books.filter {
                it.serverInstanceId == serverInstanceId && it.userId == userId
            }
            books - removed.toSet()
        }
        return removed
    }

    fun root(id: String): File {
        require(validBookId(id)) { "The local EPUB id is invalid" }
        return checkNotNull(safeChild(directory, id)) { "The local EPUB path escaped its catalogue" }
    }

    fun staging(id: String): File {
        val target = File(directory, "$id.incoming")
        target.deleteRecursively()
        check(target.mkdirs()) { "Cinema could not create the EPUB staging directory" }
        return target
    }

    fun discardStaging(id: String) {
        File(directory, "$id.incoming").deleteRecursively()
    }

    fun publish(staging: File, id: String): File {
        val destination = root(id)
        destination.deleteRecursively()
        check(staging.renameTo(destination)) { "Cinema could not publish the local EPUB atomically" }
        return destination
    }

    /** Resolve the two process-death windows: before and after atomic rename. */
    suspend fun reconcile() {
        val snapshot = records.value
        for (book in snapshot) {
            when {
                book.state in setOf("intent", "downloading") && complete(book, root(book.id)) -> {
                    val bytes = root(book.id).walkTopDown()
                        .filter(File::isFile)
                        .sumOf(File::length)
                    update(book.id) {
                        it.copy(
                            state = "downloaded",
                            phase = "ready",
                            bytesDownloaded = bytes,
                            bytesTotal = bytes,
                            errorMessage = null,
                        )
                    }
                }
                book.state in setOf("intent", "downloading") -> {
                    discardStaging(book.id)
                    update(book.id) {
                        it.copy(
                            state = "failed",
                            phase = "interrupted",
                            errorMessage = "Download interrupted — download again",
                        )
                    }
                }
                book.state == "downloaded" && !complete(book, root(book.id)) -> update(book.id) {
                    it.copy(
                        state = "missing",
                        phase = "missing",
                        errorMessage = "Download missing — download again",
                    )
                }
            }
        }
    }

    internal fun complete(book: OfflineBook, root: File): Boolean {
        val revision = book.revision ?: return false
        val publication = book.publication ?: return false
        val limits = book.limits ?: return false
        val original = File(root, "book.epub")
        val manifestFile = File(root, "publication.json")
        if (!original.isFile || original.length() != revision.size || !manifestFile.isFile) return false
        val saved = runCatching {
            json.decodeFromString<PublicationManifest>(manifestFile.readText())
        }.getOrNull() ?: return false
        if (saved != publication) return false

        val links = publication.readingOrder + publication.resources
        val resolved = links.map { safePublicationPath(it.href) ?: return false }
        val paths = resolved.toSet()
        if (paths.size > limits.entries) return false
        val resourceRoot = File(root, "publication")
        var total = 0L
        for (path in paths) {
            val resource = safeChild(resourceRoot, path) ?: return false
            if (!resource.isFile || resource.length() > limits.resource_bytes) return false
            total = runCatching { Math.addExact(total, resource.length()) }.getOrNull() ?: return false
            if (total > limits.total_uncompressed_bytes) return false
        }
        return true
    }

    internal fun safeChild(root: File, path: String): File? {
        val base = runCatching { root.canonicalFile }.getOrNull() ?: return null
        val child = runCatching { File(base, path).canonicalFile }.getOrNull() ?: return null
        val prefix = base.path.trimEnd(File.separatorChar) + File.separator
        return child.takeIf { it.path.startsWith(prefix) }
    }

    private suspend fun mutate(change: (List<OfflineBook>) -> List<OfflineBook>) {
        mutex.withLock {
            val updated = change(_records.value)
            withContext(Dispatchers.IO) { write(updated) }
            _records.value = updated
        }
    }

    private fun readInitial(): List<OfflineBook> {
        for (candidate in listOf(index, backup)) {
            val decoded = runCatching {
                if (!candidate.isFile) null
                else json.decodeFromString(serializer, candidate.readText())
            }.getOrNull()
            if (decoded != null) return decoded
                .filter { validBookId(it.id) }
                .distinctBy(OfflineBook::id)
        }
        return emptyList()
    }

    private fun write(books: List<OfflineBook>) {
        directory.mkdirs()
        val temporary = File(directory, "catalog.json.tmp")
        FileOutputStream(temporary).use { output ->
            output.write(json.encodeToString(serializer, books).toByteArray())
            output.fd.sync()
        }
        if (index.isFile) index.copyTo(backup, overwrite = true)
        check(temporary.renameTo(index)) { "Cinema could not commit the EPUB catalogue" }
    }

    private fun validBookId(id: String): Boolean =
        id.length in 1..128 && id !in setOf(".", "..") &&
            id.all { it.isLetterOrDigit() || it in setOf('-', '_', '.') }
}
