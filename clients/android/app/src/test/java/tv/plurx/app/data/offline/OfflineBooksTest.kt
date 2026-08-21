package tv.plurx.app.data.offline

import kotlinx.coroutines.runBlocking
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tv.plurx.app.data.PublicationLimits
import tv.plurx.app.data.PublicationLink
import tv.plurx.app.data.PublicationManifest
import tv.plurx.app.data.PublicationMetadata
import tv.plurx.app.data.ReadingRevision
import java.io.File
import java.nio.file.Files

class OfflineBooksTest {
    @Test
    fun publicationPathsPreservePlusAndRefuseEncodedTraversal() {
        assertEquals("Text/a+b.xhtml", safePublicationPath("Text/a+b.xhtml#page"))
        assertEquals("Text/a+b.xhtml", safePublicationPath("Text/a%2Bb.xhtml"))
        assertEquals("chapter.xhtml", safePublicationPath("Text/../chapter.xhtml"))
        assertNull(safePublicationPath("../secret"))
        assertNull(safePublicationPath("Text/%2Fsecret"))
        assertNull(safePublicationPath("Text/%5Csecret"))
        assertNull(safePublicationPath("https:%2F%2Fevil.invalid/book"))
    }

    @Test
    fun catalogIsProfileScopedAndSurvivesAReopen() = withDirectory { directory ->
        val catalog = OfflineBookCatalog(directory)
        runBlocking {
            catalog.upsert(book(id = "one", server = "alpha", user = 7))
            catalog.upsert(book(id = "two", server = "alpha", user = 8))
        }

        val reopened = OfflineBookCatalog(directory)
        assertEquals(listOf("one"), reopened.profile("alpha", 7).map(OfflineBook::id))
        assertEquals(listOf("two"), reopened.profile("alpha", 8).map(OfflineBook::id))
        assertTrue(reopened.profile("beta", 7).isEmpty())
    }

    @Test
    fun pendingReplayKeepsTheNewestWriteForEveryExactEdition() {
        val revision = ReadingRevision(size = 4, mtime = 9)
        val pending = newestPendingBooks(
            listOf(
                book("old", "alpha", 7, revision, fileId = 11, recordedAt = 40),
                book("new", "alpha", 7, revision, fileId = 11, recordedAt = 70),
                book("edition", "alpha", 7, revision, fileId = 12, recordedAt = 60),
                book("settled", "alpha", 7, revision, fileId = 13, recordedAt = 90, pending = false),
            ),
        )
        assertEquals(listOf("edition", "new"), pending.map(OfflineBook::id))
    }

    @Test
    fun completedPublicationRequiresOriginalManifestAndEveryBoundedResource() =
        withDirectory { directory ->
            val catalog = OfflineBookCatalog(directory)
            val publication = PublicationManifest(
                metadata = PublicationMetadata(title = "Offline contract"),
                readingOrder = listOf(PublicationLink("Text/chapter.xhtml", "application/xhtml+xml")),
                resources = listOf(PublicationLink("Styles/book.css", "text/css")),
            )
            val root = catalog.root("complete").apply { mkdirs() }
            val publicationRoot = File(root, "publication")
            File(publicationRoot, "Text").mkdirs()
            File(publicationRoot, "Styles").mkdirs()
            File(root, "book.epub").writeBytes(byteArrayOf(1, 2, 3, 4))
            File(publicationRoot, "Text/chapter.xhtml").writeText("<p>hello</p>")
            File(publicationRoot, "Styles/book.css").writeText("p{color:black}")
            File(root, "publication.json").writeText(Json.encodeToString(publication))
            val complete = book(
                id = "complete",
                server = "alpha",
                user = 7,
                revision = ReadingRevision(size = 4, mtime = 9),
                publication = publication,
                limits = PublicationLimits(
                    entries = 2,
                    total_uncompressed_bytes = 1_024,
                    resource_bytes = 512,
                    markup_bytes = 512,
                    compression_ratio = 100,
                    concurrent_resource_reads = 2,
                    resource_chunk_bytes = 64,
                ),
                state = "downloaded",
            )

            assertTrue(catalog.complete(complete, root))
            File(publicationRoot, "Styles/book.css").delete()
            assertFalse(catalog.complete(complete, root))
            assertNull(catalog.safeChild(publicationRoot, "../book.epub"))
        }

    private fun book(
        id: String,
        server: String,
        user: Long,
        revision: ReadingRevision? = null,
        publication: PublicationManifest? = null,
        limits: PublicationLimits? = null,
        state: String = "intent",
        fileId: Long = 11,
        recordedAt: Long? = null,
        pending: Boolean = recordedAt != null,
    ) = OfflineBook(
        id = id,
        serverInstanceId = server,
        userId = user,
        itemId = 10,
        fileId = fileId,
        revision = revision,
        title = "Book",
        publication = publication,
        limits = limits,
        state = state,
        recordedAt = recordedAt,
        pendingProgress = pending,
    )

    private fun withDirectory(test: (File) -> Unit) {
        val directory = Files.createTempDirectory("cinema-offline-books-").toFile()
        try {
            test(directory)
        } finally {
            directory.deleteRecursively()
        }
    }
}
