#if os(iOS)
import Foundation

/// Durable, profile-scoped truth for original EPUB downloads and their
/// renderer-neutral reading state. The catalog never depends on the server to
/// list or open a completed book.
actor OfflineBookCatalog {
    private let fileManager: FileManager
    private let directory: URL
    private let indexURL: URL
    private let backupURL: URL
    private var books: [String: OfflineBook]

    init(fileManager: FileManager = .default, directory injectedDirectory: URL? = nil) {
        let support = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let directory = injectedDirectory
            ?? support.appendingPathComponent("OfflineBooks", isDirectory: true)
        let indexURL = directory.appendingPathComponent("index.json")
        let backupURL = directory.appendingPathComponent("index.backup.json")
        try? fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        var excluded = directory
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try? excluded.setResourceValues(values)
        let loaded = Self.decode(at: indexURL) ?? Self.decode(at: backupURL) ?? []
        self.fileManager = fileManager
        self.directory = directory
        self.indexURL = indexURL
        self.backupURL = backupURL
        self.books = Dictionary(uniqueKeysWithValues: loaded.map { ($0.id, $0) })
    }

    func currentProfile(serverInstanceId: String, userId: Int) -> [OfflineBook] {
        books.values.filter {
            $0.serverInstanceId == serverInstanceId && $0.userId == userId
        }.sorted { $0.updatedAt > $1.updatedAt }
    }

    func allBooks() -> [OfflineBook] { Array(books.values) }
    func book(id: String) -> OfflineBook? { books[id] }

    func book(serverInstanceId: String, userId: Int, fileId: Int) -> OfflineBook? {
        books.values.filter {
            $0.serverInstanceId == serverInstanceId && $0.userId == userId && $0.fileId == fileId
        }.max { $0.updatedAt < $1.updatedAt }
    }

    func upsert(_ book: OfflineBook) throws {
        books[book.id] = book
        try persist()
    }

    @discardableResult
    func update(id: String, _ change: (inout OfflineBook) -> Bool) throws -> OfflineBook? {
        guard var book = books[id], change(&book) else { return nil }
        book.updatedAt = Date()
        books[id] = book
        try persist()
        return book
    }

    func remove(id: String) throws -> OfflineBook? {
        let removed = books.removeValue(forKey: id)
        try persist()
        return removed
    }

    func removeProfile(serverInstanceId: String, userId: Int) throws -> [OfflineBook] {
        let removed = books.values.filter {
            $0.serverInstanceId == serverInstanceId && $0.userId == userId
        }
        removed.forEach { books.removeValue(forKey: $0.id) }
        try persist()
        return removed
    }

    func otherProfiles(serverInstanceId: String?, userId: Int?) -> [OfflineProfileSummary] {
        var summaries: [String: OfflineProfileSummary] = [:]
        for book in books.values {
            if book.serverInstanceId == serverInstanceId && book.userId == userId { continue }
            let key = "\(book.serverInstanceId):\(book.userId)"
            let current = summaries[key]
            summaries[key] = OfflineProfileSummary(
                serverInstanceId: book.serverInstanceId,
                userId: book.userId,
                items: (current?.items ?? 0) + 1,
                bytes: (current?.bytes ?? 0) + max(0, book.bytesTotal)
            )
        }
        return summaries.values.sorted { $0.id < $1.id }
    }

    func newestPending(serverInstanceId: String, userId: Int) -> [OfflineBook] {
        var newest: [Int: OfflineBook] = [:]
        for book in books.values where book.serverInstanceId == serverInstanceId
            && book.userId == userId && book.pendingProgress {
            if (newest[book.itemId]?.recordedAt ?? .min) < (book.recordedAt ?? .min) {
                newest[book.itemId] = book
            }
        }
        return newest.values.sorted { ($0.recordedAt ?? 0) < ($1.recordedAt ?? 0) }
    }

    /// An OS purge or manual filesystem change becomes an honest retry state.
    func reconcileLocalPublications() throws {
        var changed = false
        for (id, var book) in books {
            if book.state == .intent || book.state == .downloading {
                let recovered = directory.appendingPathComponent(id, isDirectory: true)
                if Self.hasRequiredFiles(book, at: recovered, fileManager: fileManager),
                   let relative = OfflineCatalog.relativeLocalPath(for: recovered) {
                    book.state = .downloaded
                    book.phase = "ready"
                    book.localPublicationRelativePath = relative
                    book.coverRelativePath = fileManager.fileExists(
                        atPath: recovered.appendingPathComponent("cover").path
                    ) ? relative + "/cover" : nil
                    book.bytesDownloaded = Self.allocatedBytes(at: recovered)
                    book.bytesTotal = book.bytesDownloaded
                    book.errorMessage = nil
                    book.updatedAt = Date()
                    books[id] = book
                    changed = true
                    continue
                }
                book.state = .failed
                book.phase = "interrupted"
                book.errorMessage = "Download interrupted — download again"
                book.updatedAt = Date()
                books[id] = book
                try? fileManager.removeItem(
                    at: directory.appendingPathComponent("\(id).incoming", isDirectory: true)
                )
                changed = true
                continue
            }
            guard book.state == .downloaded else { continue }
            guard let relative = book.localPublicationRelativePath,
                  Self.hasRequiredFiles(
                    book,
                    at: OfflineCatalog.localURL(for: relative),
                    fileManager: fileManager
                  )
            else {
                book.state = .missing
                book.errorMessage = "Download missing — download again"
                book.updatedAt = Date()
                books[id] = book
                changed = true
                continue
            }
        }
        if changed { try persist() }
    }

    func stagingDirectory(id: String) throws -> URL {
        let incoming = directory.appendingPathComponent("\(id).incoming", isDirectory: true)
        if fileManager.fileExists(atPath: incoming.path) { try fileManager.removeItem(at: incoming) }
        try fileManager.createDirectory(at: incoming, withIntermediateDirectories: true)
        return incoming
    }

    func publish(staging: URL, id: String) throws -> String {
        let destination = directory.appendingPathComponent(id, isDirectory: true)
        if fileManager.fileExists(atPath: destination.path) { try fileManager.removeItem(at: destination) }
        try fileManager.moveItem(at: staging, to: destination)
        guard let relative = OfflineCatalog.relativeLocalPath(for: destination) else {
            throw APIError.transport("Cinema could not publish the local EPUB safely.")
        }
        return relative
    }

    func discardStaging(id: String) {
        try? fileManager.removeItem(at: directory.appendingPathComponent("\(id).incoming"))
    }

    private static func decode(at url: URL) -> [OfflineBook]? {
        guard let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode([OfflineBook].self, from: data)
    }

    private static func hasRequiredFiles(
        _ book: OfflineBook,
        at root: URL,
        fileManager: FileManager
    ) -> Bool {
        let original = root.appendingPathComponent("book.epub")
        let manifestURL = root.appendingPathComponent("publication.json")
        guard let publication = book.publication,
              let revision = book.revision,
              let limits = book.limits,
              isRegularFile(original, fileManager: fileManager),
              isRegularFile(manifestURL, fileManager: fileManager),
              let originalSize = fileSize(original, fileManager: fileManager),
              originalSize == Int64(revision.size),
              let manifestData = try? Data(contentsOf: manifestURL),
              let savedManifest = try? JSONDecoder().decode(
                PublicationManifest.self,
                from: manifestData
              ),
              savedManifest == publication
        else { return false }
        let resourceRoot = root.appendingPathComponent("publication", isDirectory: true)
        let links = publication.readingOrder + publication.resources
        let resolved = links.compactMap {
            OfflineBookManager.safePublicationPath($0.href)
        }
        let paths = Set(resolved)
        guard resolved.count == links.count, paths.count <= limits.entries else { return false }
        var total: Int64 = 0
        for path in paths {
            let file = resourceRoot.appendingPathComponent(path)
            guard isRegularFile(file, fileManager: fileManager),
                  let bytes = fileSize(file, fileManager: fileManager),
                  bytes <= limits.resourceBytes else { return false }
            let (next, overflow) = total.addingReportingOverflow(bytes)
            guard !overflow, next <= limits.totalUncompressedBytes else { return false }
            total = next
        }
        return true
    }

    private static func isRegularFile(_ url: URL, fileManager: FileManager) -> Bool {
        var directory: ObjCBool = false
        return fileManager.fileExists(atPath: url.path, isDirectory: &directory) && !directory.boolValue
    }

    private static func fileSize(_ url: URL, fileManager: FileManager) -> Int64? {
        let attributes = try? fileManager.attributesOfItem(atPath: url.path)
        return (attributes?[.size] as? NSNumber)?.int64Value
    }

    private static func allocatedBytes(at root: URL) -> Int64 {
        guard let enumerator = FileManager.default.enumerator(
            at: root,
            includingPropertiesForKeys: [.fileAllocatedSizeKey],
            options: [.skipsHiddenFiles]
        ) else { return 0 }
        return enumerator.reduce(into: Int64(0)) { total, value in
            guard let url = value as? URL,
                  let bytes = try? url.resourceValues(
                    forKeys: [.fileAllocatedSizeKey]
                  ).fileAllocatedSize else { return }
            total += Int64(bytes)
        }
    }

    private func persist() throws {
        if fileManager.fileExists(atPath: indexURL.path) {
            try? fileManager.removeItem(at: backupURL)
            try? fileManager.copyItem(at: indexURL, to: backupURL)
        }
        let data = try JSONEncoder().encode(books.values.sorted { $0.id < $1.id })
        try data.write(to: indexURL, options: .atomic)
    }
}
#endif
