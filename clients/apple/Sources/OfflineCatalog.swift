import Foundation

struct OfflineProfileSummary: Identifiable, Equatable {
    let serverInstanceId: String
    let userId: Int
    let items: Int
    let bytes: Int64

    var id: String { "\(serverInstanceId):\(userId)" }
}

/// Durable local truth for app-managed downloads. Network responses enrich a
/// row, but no screen needs the server to render a completed offline library.
actor OfflineCatalog {
    private let fileManager: FileManager
    private let directory: URL
    private let indexURL: URL
    private let backupURL: URL
    private var items: [String: OfflineItem]

    init(fileManager: FileManager = .default, directory injectedDirectory: URL? = nil) {
        let support = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let directory = injectedDirectory
            ?? support.appendingPathComponent("Offline", isDirectory: true)
        let indexURL = directory.appendingPathComponent("index.json")
        let backupURL = directory.appendingPathComponent("index.backup.json")
        try? fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        var offlineDirectory = directory
        var resourceValues = URLResourceValues()
        resourceValues.isExcludedFromBackup = true
        try? offlineDirectory.setResourceValues(resourceValues)
        let loaded = Self.decodeIndex(at: indexURL)
            ?? Self.decodeIndex(at: backupURL)
            ?? [:]
        self.fileManager = fileManager
        self.directory = directory
        self.indexURL = indexURL
        self.backupURL = backupURL
        self.items = loaded
    }

    func profileItems(serverInstanceId: String, userId: Int) -> [OfflineItem] {
        items.values
            .filter { $0.serverInstanceId == serverInstanceId && $0.userId == userId }
            .sorted { $0.updatedAt > $1.updatedAt }
    }

    func item(id: String) -> OfflineItem? { items[id] }

    func allItems() -> [OfflineItem] { Array(items.values) }

    func item(serverInstanceId: String, userId: Int, fileId: Int) -> OfflineItem? {
        items.values.first {
            $0.serverInstanceId == serverInstanceId
                && $0.userId == userId
                && $0.fileId == fileId
        }
    }

    func upsert(_ item: OfflineItem) throws {
        items[item.id] = item
        try persist()
    }

    func remove(id: String) throws -> OfflineItem? {
        let removed = items.removeValue(forKey: id)
        try persist()
        return removed
    }

    func replace(_ item: OfflineItem) throws {
        guard items[item.id] != nil else { return }
        items[item.id] = item
        try persist()
    }

    /// Read-modify-write without an actor reentrancy gap. Delegate callbacks
    /// must never carry a stale item snapshot across an await and replace a
    /// terminal state written by a later callback.
    @discardableResult
    func update(
        id: String,
        _ change: (inout OfflineItem) -> Bool
    ) throws -> OfflineItem? {
        guard var item = items[id], change(&item) else { return nil }
        item.updatedAt = Date()
        items[id] = item
        try persist()
        return item
    }

    func otherProfiles(serverInstanceId: String?, userId: Int?) -> [OfflineProfileSummary] {
        var groups: [String: OfflineProfileSummary] = [:]
        for item in items.values {
            if item.serverInstanceId == serverInstanceId && item.userId == userId { continue }
            let key = "\(item.serverInstanceId):\(item.userId)"
            let current = groups[key]
            groups[key] = OfflineProfileSummary(
                serverInstanceId: item.serverInstanceId,
                userId: item.userId,
                items: (current?.items ?? 0) + 1,
                bytes: (current?.bytes ?? 0) + max(0, item.bytesTotal ?? item.bytesDownloaded)
            )
        }
        return groups.values.sorted { $0.id < $1.id }
    }

    func removeProfile(serverInstanceId: String, userId: Int) throws -> [OfflineItem] {
        let removed = items.values.filter {
            $0.serverInstanceId == serverInstanceId && $0.userId == userId
        }
        for item in removed { items.removeValue(forKey: item.id) }
        try persist()
        return removed
    }

    /// Verify the OS-managed package still exists. Managed storage is an
    /// eviction hint, never a promise, so a missing asset becomes an honest
    /// recoverable state rather than a play button that fails later.
    func reconcileLocalAssets() throws {
        var changed = false
        for (id, var item) in items where item.state == .downloaded {
            guard let path = item.localAssetRelativePath,
                  fileManager.fileExists(atPath: Self.localURL(for: path).path)
            else {
                item.state = .missing
                item.errorMessage = "Download missing — download again"
                item.updatedAt = Date()
                items[id] = item
                changed = true
                continue
            }
        }
        if changed { try persist() }
    }

    func newestPendingProgress(serverInstanceId: String, userId: Int) -> [OfflineItem] {
        var newest: [Int: OfflineItem] = [:]
        for item in items.values where item.serverInstanceId == serverInstanceId
            && item.userId == userId && item.pendingProgress {
            if newest[item.itemId].map({ $0.recordedAt ?? .distantPast }) ?? .distantPast
                < (item.recordedAt ?? .distantPast) {
                newest[item.itemId] = item
            }
        }
        return newest.values.sorted { ($0.recordedAt ?? .distantPast) < ($1.recordedAt ?? .distantPast) }
    }

    static func relativeLocalPath(for url: URL) -> String? {
        relativeLocalPath(
            for: url,
            homeDirectory: URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
        )
    }

    /// AVFoundation and Foundation can spell the same iOS data container as
    /// `/private/var/...` and `/var/...`. Compare normalized path components
    /// rather than raw strings so that alias cannot discard a completed asset.
    /// Exact components also keep a sibling such as `<container>-other` from
    /// passing the descendant check.
    static func relativeLocalPath(for url: URL, homeDirectory: URL) -> String? {
        let root = comparablePathComponents(for: homeDirectory)
        let target = comparablePathComponents(for: url)
        guard target.count > root.count,
              target.starts(with: root)
        else { return nil }
        return target.dropFirst(root.count).joined(separator: "/")
    }

    static func localURL(for relativePath: String) -> URL {
        localURL(
            for: relativePath,
            homeDirectory: URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
        )
    }

    static func localURL(for relativePath: String, homeDirectory: URL) -> URL {
        homeDirectory.appendingPathComponent(relativePath)
    }

    private static func comparablePathComponents(for url: URL) -> [String] {
        let components = url.standardizedFileURL.pathComponents
        guard components.count > 2,
              components[0] == "/",
              components[1] == "private",
              components[2] == "var"
        else { return components }
        return ["/"] + Array(components.dropFirst(2))
    }

    private static func decodeIndex(at url: URL) -> [String: OfflineItem]? {
        guard let data = try? Data(contentsOf: url),
              let decoded = try? JSONDecoder().decode([OfflineItem].self, from: data)
        else { return nil }
        return Dictionary(uniqueKeysWithValues: decoded.map { ($0.id, $0) })
    }

    private func persist() throws {
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        if fileManager.fileExists(atPath: indexURL.path) {
            try? fileManager.removeItem(at: backupURL)
            try? fileManager.copyItem(at: indexURL, to: backupURL)
        }
        let data = try JSONEncoder().encode(items.values.sorted { $0.id < $1.id })
        try data.write(to: indexURL, options: .atomic)
    }
}
