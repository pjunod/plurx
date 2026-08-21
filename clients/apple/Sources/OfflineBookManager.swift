#if os(iOS)
import Combine
import Foundation

private actor OfflineBookTransferRegistry {
    private var cancelled: Set<String> = []
    func begin(_ id: String) { cancelled.remove(id) }
    func cancel(_ id: String) { cancelled.insert(id) }
    func check(_ id: String) throws {
        if cancelled.contains(id) { throw CancellationError() }
    }
    func finish(_ id: String) { cancelled.remove(id) }
}

/// Downloads an original EPUB and a bounded local rendering cache without the
/// video offline-package queue. A catalog intent exists before the first
/// request; publication becomes visible only after one same-volume rename.
final class OfflineBookManager: ObservableObject {
    static let shared = OfflineBookManager()

    @Published private(set) var books: [OfflineBook] = []
    @Published private(set) var otherProfiles: [OfflineProfileSummary] = []

    let catalog: OfflineBookCatalog
    private let settings: SettingsStore
    private let session: URLSession
    private let transfers = OfflineBookTransferRegistry()

    init(
        catalog: OfflineBookCatalog = OfflineBookCatalog(),
        settings: SettingsStore = SettingsStore(),
        session: URLSession? = nil
    ) {
        self.catalog = catalog
        self.settings = settings
        if let session {
            self.session = session
        } else {
            let configuration = URLSessionConfiguration.default
            configuration.waitsForConnectivity = true
            configuration.timeoutIntervalForRequest = 90
            configuration.timeoutIntervalForResource = 60 * 60
            configuration.allowsExpensiveNetworkAccess = settings.offlineNetwork == .anyNetwork
            self.session = URLSession(configuration: configuration)
        }
        Task {
            try? await catalog.reconcileLocalPublications()
            await refresh()
        }
    }

    @MainActor
    func refresh() async {
        let current: [OfflineBook]
        if let instance = settings.instanceId, let user = settings.userId {
            current = await catalog.currentProfile(serverInstanceId: instance, userId: user)
        } else {
            current = []
        }
        books = current
        otherProfiles = await catalog.otherProfiles(
            serverInstanceId: settings.instanceId,
            userId: settings.userId
        )
    }

    func queue(
        itemId: Int,
        file: MediaFile,
        title: String,
        posterPath: String?
    ) async throws {
        guard let instance = settings.instanceId, let user = settings.userId,
              !settings.origin.isEmpty else {
            throw APIError.transport("Connect once before downloading")
        }
        if let existing = await catalog.book(
            serverInstanceId: instance,
            userId: user,
            fileId: file.id
        ) {
            if existing.isPlayable { return }
            await remove(existing)
        }

        let id = UUID().uuidString
        let book = OfflineBook(
            id: id,
            serverInstanceId: instance,
            userId: user,
            itemId: itemId,
            fileId: file.id,
            revision: nil,
            title: title,
            author: nil,
            originalFilename: file.filename ?? "book.epub",
            coverRelativePath: nil,
            publication: nil,
            limits: nil,
            state: .intent,
            phase: "opening_publication",
            bytesDownloaded: 0,
            bytesTotal: Int64(max(0, file.size ?? 0)),
            localPublicationRelativePath: nil,
            locator: nil,
            progression: 0,
            completed: false,
            recordedAt: nil,
            pendingProgress: false,
            preferences: OfflineBookPreferences(),
            errorMessage: nil,
            updatedAt: Date()
        )
        try await catalog.upsert(book)
        await refresh()
        await transfers.begin(id)
        let api = PlurxAPI(origin: settings.origin)
        var sessionsToClose: [String] = []

        do {
            var opened = try await api.openPublication(fileId: file.id)
            sessionsToClose.append(opened.sessionId)
            try await transfers.check(id)
            try Self.requireSpace(for: opened)
            _ = try await catalog.update(id: id) { current in
                current.revision = opened.revision
                current.publication = opened.publication
                current.limits = opened.limits
                current.title = opened.publication.metadata.title.isEmpty
                    ? title : opened.publication.metadata.title
                current.author = opened.publication.metadata.author
                current.state = .downloading
                current.phase = "downloading_original"
                current.bytesTotal = Int64(opened.revision.size)
                    + opened.limits.totalUncompressedBytes
                return true
            }
            await refresh()

            let staging = try await catalog.stagingDirectory(id: id)
            do {
                let original = staging.appendingPathComponent("book.epub")
                let originalBytes = try await download(
                    try api.bookContentRequest(fileId: file.id),
                    to: original,
                    maximumBytes: Int64(opened.revision.size)
                )
                guard originalBytes == Int64(opened.revision.size) else {
                    throw APIError.transport("The EPUB changed while Cinema downloaded it.")
                }
                try await transfers.check(id)

                // A large original can outlive the first short publication
                // capability. Reopen and require the exact same revision before
                // downloading any renderer resource.
                try? await api.closePublication(sessionId: opened.sessionId)
                sessionsToClose.removeAll { $0 == opened.sessionId }
                let reopened = try await api.openPublication(fileId: file.id)
                sessionsToClose.append(reopened.sessionId)
                guard reopened.revision == opened.revision,
                      reopened.publication == opened.publication else {
                    throw APIError.transport("The EPUB changed while Cinema downloaded it.")
                }
                opened = reopened

                let publicationRoot = staging.appendingPathComponent("publication", isDirectory: true)
                try FileManager.default.createDirectory(
                    at: publicationRoot,
                    withIntermediateDirectories: true
                )
                let resources = try Self.publicationResources(opened)
                var extractedBytes: Int64 = 0
                for (offset, resource) in resources.enumerated() {
                    try await transfers.check(id)
                    let target = publicationRoot.appendingPathComponent(resource.path)
                    try FileManager.default.createDirectory(
                        at: target.deletingLastPathComponent(),
                        withIntermediateDirectories: true
                    )
                    let request = try api.publicationResourceRequest(
                        base: opened.resourceBase,
                        path: Self.percentEncodedPath(resource.path)
                    )
                    let bytes = try await download(
                        request,
                        to: target,
                        maximumBytes: opened.limits.resourceBytes
                    )
                    extractedBytes = try Self.boundedAdd(
                        extractedBytes,
                        bytes,
                        maximum: opened.limits.totalUncompressedBytes
                    )
                    _ = try await catalog.update(id: id) { current in
                        current.phase = "downloading_resources_\(offset + 1)_of_\(resources.count)"
                        current.bytesDownloaded = originalBytes + extractedBytes
                        return true
                    }
                    if offset % 8 == 0 || offset + 1 == resources.count { await refresh() }
                }
                let manifest = try JSONEncoder().encode(opened.publication)
                try manifest.write(
                    to: staging.appendingPathComponent("publication.json"),
                    options: .atomic
                )
                let cachedCover = (try? await cacheCover(posterPath, in: staging)) != nil
                try await transfers.check(id)
                let relative = try await catalog.publish(staging: staging, id: id)
                do {
                    let published = try await catalog.update(id: id) { current in
                        current.state = .downloaded
                        current.phase = "ready"
                        current.bytesDownloaded = originalBytes + extractedBytes
                        current.bytesTotal = originalBytes + extractedBytes
                        current.localPublicationRelativePath = relative
                        current.coverRelativePath = cachedCover ? relative + "/cover" : nil
                        current.errorMessage = nil
                        return true
                    }
                    guard published != nil else { throw CancellationError() }
                } catch {
                    try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: relative))
                    throw error
                }
            } catch {
                await catalog.discardStaging(id: id)
                throw error
            }
            await refresh()
        } catch is CancellationError {
            await catalog.discardStaging(id: id)
            for sessionId in sessionsToClose { try? await api.closePublication(sessionId: sessionId) }
            await transfers.finish(id)
            throw CancellationError()
        } catch {
            if await catalog.book(id: id) != nil {
                _ = try? await catalog.update(id: id) { current in
                    current.state = .failed
                    current.phase = "failed"
                    current.errorMessage = error.localizedDescription
                    return true
                }
                await refresh()
            }
            for sessionId in sessionsToClose { try? await api.closePublication(sessionId: sessionId) }
            await transfers.finish(id)
            throw error
        }
        for sessionId in sessionsToClose { try? await api.closePublication(sessionId: sessionId) }
        await transfers.finish(id)
    }

    func remove(_ book: OfflineBook) async {
        await transfers.cancel(book.id)
        if let relative = book.localPublicationRelativePath {
            try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: relative))
        }
        await catalog.discardStaging(id: book.id)
        _ = try? await catalog.remove(id: book.id)
        await refresh()
    }

    func removeProfile(_ profile: OfflineProfileSummary) async {
        let removed = (try? await catalog.removeProfile(
            serverInstanceId: profile.serverInstanceId,
            userId: profile.userId
        )) ?? []
        for book in removed {
            await transfers.cancel(book.id)
            if let relative = book.localPublicationRelativePath {
                try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: relative))
            }
            await catalog.discardStaging(id: book.id)
        }
        await refresh()
    }

    func hasCurrentProfileDownloads() async -> Bool {
        guard let instance = settings.instanceId, let user = settings.userId else { return false }
        return !(await catalog.currentProfile(serverInstanceId: instance, userId: user)).isEmpty
    }

    func removeCurrentProfile() async {
        guard let instance = settings.instanceId, let user = settings.userId else { return }
        let current = await catalog.currentProfile(serverInstanceId: instance, userId: user)
        for book in current { await remove(book) }
    }

    func record(
        bookId: String,
        locator: ReadingLocator,
        progression: Double,
        completed: Bool,
        recordedAt: Int,
        preferences: OfflineBookPreferences
    ) async {
        _ = try? await catalog.update(id: bookId) { book in
            guard recordedAt >= (book.recordedAt ?? .min) else { return false }
            book.locator = locator
            book.progression = min(1, max(0, progression))
            book.completed = completed
            book.recordedAt = recordedAt
            book.pendingProgress = true
            book.preferences = preferences
            return true
        }
        await refresh()
    }

    func syncPendingProgress() async {
        guard let instance = settings.instanceId, let user = settings.userId,
              !settings.origin.isEmpty, Session.shared.token != nil else { return }
        let pending = await catalog.newestPending(serverInstanceId: instance, userId: user)
        let api = PlurxAPI(origin: settings.origin)
        for snapshot in pending {
            guard let revision = snapshot.revision, let locator = snapshot.locator,
                  let recordedAt = snapshot.recordedAt else { continue }
            do {
                _ = try await api.putReadingState(
                    itemId: snapshot.itemId,
                    state: PutReadingStateRequest(
                        fileId: snapshot.fileId,
                        revision: revision,
                        locator: locator,
                        progression: snapshot.progression,
                        completed: snapshot.completed,
                        recordedAt: recordedAt
                    )
                )
                _ = try await catalog.update(id: snapshot.id) { current in
                    guard current.recordedAt == recordedAt, current.locator == locator else {
                        return false
                    }
                    current.pendingProgress = false
                    current.errorMessage = nil
                    return true
                }
            } catch {
                // Offline is the normal case. Keep the newest durable write for
                // the next foreground/reconnect pass.
            }
        }
        await refresh()
    }

    private func download(
        _ request: URLRequest,
        to destination: URL,
        maximumBytes: Int64
    ) async throws -> Int64 {
        let (temporary, response) = try await session.download(for: request)
        guard let http = response as? HTTPURLResponse,
              (200..<300).contains(http.statusCode) else {
            throw APIError.http((response as? HTTPURLResponse)?.statusCode ?? 0)
        }
        let attributes = try FileManager.default.attributesOfItem(atPath: temporary.path)
        let bytes = (attributes[.size] as? NSNumber)?.int64Value ?? 0
        guard bytes >= 0 && bytes <= maximumBytes else {
            throw APIError.transport("The EPUB resource exceeds Cinema's safety limit.")
        }
        if FileManager.default.fileExists(atPath: destination.path) {
            try FileManager.default.removeItem(at: destination)
        }
        try FileManager.default.moveItem(at: temporary, to: destination)
        return bytes
    }

    private func cacheCover(_ path: String?, in staging: URL) async throws -> String? {
        guard let path, let url = Session.shared.mediaURL(path) else { return nil }
        var request = URLRequest(url: url)
        Session.shared.authorize(&request)
        let target = staging.appendingPathComponent("cover")
        _ = try await download(request, to: target, maximumBytes: 32 * 1_024 * 1_024)
        return "cover"
    }

    private struct Resource {
        let path: String
    }

    static func safePublicationPath(_ href: String) -> String? {
        let path = href.split(separator: "#", maxSplits: 1, omittingEmptySubsequences: false)[0]
        var output: [String] = []
        for raw in path.split(separator: "/", omittingEmptySubsequences: true) {
            let part = String(raw).removingPercentEncoding ?? String(raw)
            if part == "." { continue }
            if part == ".." {
                guard !output.isEmpty else { return nil }
                output.removeLast()
            } else {
                guard !part.isEmpty, !part.contains("/"), !part.contains("\\"),
                      !part.contains(":"), part != ".",
                      part.unicodeScalars.allSatisfy({ $0.value >= 0x20 && $0.value != 0x7f })
                else {
                    return nil
                }
                output.append(part)
            }
        }
        return output.isEmpty ? nil : output.joined(separator: "/")
    }

    private static func publicationResources(_ opened: OpenPublicationResponse) throws -> [Resource] {
        var types: [String: String] = [:]
        for link in opened.publication.readingOrder + opened.publication.resources {
            guard let path = safePublicationPath(link.href) else {
                throw APIError.transport("The EPUB manifest contains an unsafe path.")
            }
            types[path] = link.type
        }
        guard types.count <= opened.limits.entries else {
            throw APIError.transport("The EPUB contains too many resources.")
        }
        return types.keys.sorted().map { Resource(path: $0) }
    }

    private static func percentEncodedPath(_ path: String) -> String {
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-._~"))
        return path.split(separator: "/").map {
            String($0).addingPercentEncoding(withAllowedCharacters: allowed) ?? ""
        }.joined(separator: "/")
    }

    private static func boundedAdd(_ left: Int64, _ right: Int64, maximum: Int64) throws -> Int64 {
        let (sum, overflow) = left.addingReportingOverflow(right)
        guard !overflow, sum <= maximum else {
            throw APIError.transport("The EPUB expands beyond Cinema's safety limit.")
        }
        return sum
    }

    private static func requireSpace(for opened: OpenPublicationResponse) throws {
        let required = Int64(opened.revision.size) + opened.limits.totalUncompressedBytes
            + 16 * 1_024 * 1_024
        let support = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        )[0]
        let available = try support.resourceValues(
            forKeys: [.volumeAvailableCapacityForImportantUsageKey]
        ).volumeAvailableCapacityForImportantUsage
        if let available, available < required {
            throw APIError.transport("Not enough device storage for this EPUB.")
        }
    }
}
#endif
