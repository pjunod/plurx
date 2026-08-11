#if os(iOS)
import AVFoundation
import Combine
import Foundation
import Security

private actor OfflinePreparationRegistry {
    private var active: Set<String> = []

    func begin(_ id: String) -> Bool { active.insert(id).inserted }
    func finish(_ id: String) { active.remove(id) }
}

/// Owns the two system background HLS sessions and maps every task back to the
/// durable local catalog. Server preparation may pause with the app; the
/// AVFoundation transfer itself is system-owned once it starts.
final class OfflineDownloadManager: NSObject, ObservableObject {
    static let shared = OfflineDownloadManager()

    @Published private(set) var items: [OfflineItem] = []
    @Published private(set) var otherProfiles: [OfflineProfileSummary] = []

    let catalog = OfflineCatalog()
    private let settings = SettingsStore()
    private let delegateQueue: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "tv.plurx.offline.asset-downloads"
        queue.maxConcurrentOperationCount = 1
        return queue
    }()
    private let stateLock = NSLock()
    private var taskLocations: [Int: URL] = [:]
    private var taskLocationWrites: [Int: Task<Void, Never>] = [:]
    private var backgroundCompletions: [String: () -> Void] = [:]
    private let preparations = OfflinePreparationRegistry()

    private var wifiSession: AVAssetDownloadURLSession!
    private var anyNetworkSession: AVAssetDownloadURLSession!

    override private init() {
        super.init()
        // The URLSession delegate is self, so construction follows super.init;
        // both references are then immutable in practice and cannot race on
        // first access from a delegate callback and foreground catch-up.
        wifiSession = makeSession(suffix: "wifi", anyNetwork: false)
        anyNetworkSession = makeSession(suffix: "any", anyNetwork: true)
        Task {
            try? await catalog.reconcileLocalAssets()
            await refresh()
            await restoreTasks()
        }
    }

    func refresh() async {
        let instance = settings.instanceId
        let user = settings.userId
        let current: [OfflineItem]
        if let instance, let user {
            current = await catalog.profileItems(serverInstanceId: instance, userId: user)
        } else {
            current = []
        }
        let others = await catalog.otherProfiles(serverInstanceId: instance, userId: user)
        await MainActor.run {
            items = current
            otherProfiles = others
        }
    }

    func queue(
        itemId: Int,
        fileId: Int,
        title: String,
        context: String?,
        durationMs: Int?,
        posterPath: String?,
        markers: [Marker] = []
    ) async throws {
        guard let instance = settings.instanceId, let user = settings.userId,
              !settings.origin.isEmpty
        else { throw APIError.transport("Connect once before downloading") }
        if let existing = await catalog.item(
            serverInstanceId: instance,
            userId: user,
            fileId: fileId
        ) {
            if existing.state != .failed && existing.state != .missing { return }
            await remove(existing)
        }

        let localId = UUID().uuidString
        var local = OfflineItem(
            id: localId,
            requestId: UUID().uuidString,
            serverInstanceId: instance,
            userId: user,
            itemId: itemId,
            fileId: fileId,
            packageId: nil,
            leaseToken: nil,
            manifestURL: nil,
            title: title,
            context: context,
            durationMs: durationMs,
            posterFile: nil,
            requestedHeight: settings.offlineQuality.maximumHeight,
            actualHeight: nil,
            audioLabel: nil,
            subtitleLabel: nil,
            subtitleIndex: nil,
            state: .intent,
            phase: "checking_options",
            bytesDownloaded: 0,
            bytesTotal: nil,
            localAssetRelativePath: nil,
            markers: markers,
            positionMs: 0,
            recordedAt: nil,
            pendingProgress: false,
            errorMessage: nil,
            updatedAt: Date()
        )
        try await catalog.upsert(local)
        await refresh()

        if let poster = await cachePoster(path: posterPath, id: localId) {
            local.posterFile = poster
            try? await catalog.replace(local)
            await refresh()
        }

        do {
            let api = PlurxAPI(origin: settings.origin)
            let options = try await api.offlineOptions(
                fileId: fileId,
                audioLanguage: settings.audioLang,
                subtitleLanguage: settings.subLang
            )
            guard let quality = options.qualities
                .filter({ $0.height <= settings.offlineQuality.maximumHeight })
                .max(by: { $0.height < $1.height })
                ?? options.qualities.min(by: { $0.height < $1.height })
            else { throw APIError.transport("No offline quality is available") }

            let audio = options.audio.first { $0.index == options.recommendedAudioIndex }
            let subtitle = options.subtitles.first {
                $0.index == options.recommendedSubtitleIndex && $0.offlineMode == "native"
            }
            local.requestedHeight = quality.height
            local.audioLabel = Self.trackLabel(language: audio?.language, title: audio?.title)
            local.subtitleLabel = Self.trackLabel(
                language: subtitle?.language,
                title: subtitle?.title
            )
            local.subtitleIndex = subtitle?.index
            local.state = .queued
            local.phase = "waiting_for_server"
            local.bytesTotal = quality.estimatedBytes
            local.updatedAt = Date()
            try await catalog.replace(local)

            let package = try await api.createOfflinePackage(
                fileId: fileId,
                body: CreateOfflinePackageRequest(
                    requestId: local.requestId,
                    height: quality.height,
                    audioIndex: options.recommendedAudioIndex,
                    subtitleIndex: subtitle?.index
                )
            )
            local.packageId = package.id
            local.actualHeight = package.output.height
            local.durationMs = package.durationMs ?? local.durationMs
            local.bytesTotal = package.actualBytes ?? package.estimatedBytes
            local.state = package.state == "ready" ? .readyToTransfer : .preparing
            local.phase = package.phase
            local.updatedAt = Date()
            try await catalog.replace(local)
            await refresh()
            try await awaitPreparation(localId: local.id, api: api)
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            if var failed = await catalog.item(id: local.id) {
                failed.state = .failed
                failed.errorMessage = Connectivity.message(
                    for: error,
                    server: settings.origin
                )
                failed.updatedAt = Date()
                try? await catalog.replace(failed)
                await refresh()
            }
            throw error
        }
    }

    /// Foreground catch-up for preparation that completed while iOS had the
    /// process suspended. The transfer becomes background-owned as soon as the
    /// lease has been converted into an AVAssetDownloadTask.
    func resumePendingPreparation() async {
        guard !settings.origin.isEmpty else { return }
        let api = PlurxAPI(origin: settings.origin)
        for item in items where [.queued, .preparing, .readyToTransfer, .paused].contains(item.state) {
            try? await awaitPreparation(localId: item.id, api: api)
        }
        await syncPendingProgress(api: api)
    }

    func resume(_ item: OfflineItem) async {
        for session in [wifiSession!, anyNetworkSession!] {
            let tasks = await allTasks(in: session)
            if let task = tasks.first(where: { $0.taskDescription == item.id }) {
                task.resume()
                if var resumed = await catalog.item(id: item.id) {
                    resumed.state = .downloading
                    resumed.phase = "downloading"
                    resumed.errorMessage = nil
                    resumed.updatedAt = Date()
                    try? await catalog.replace(resumed)
                }
                await refresh()
                return
            }
        }

        guard !settings.origin.isEmpty else { return }
        try? await awaitPreparation(
            localId: item.id,
            api: PlurxAPI(origin: settings.origin)
        )
    }

    func remove(_ item: OfflineItem) async {
        for session in [wifiSession!, anyNetworkSession!] {
            let tasks = await allTasks(in: session)
            tasks.filter { $0.taskDescription == item.id }.forEach { $0.cancel() }
        }
        if let path = item.localAssetRelativePath {
            try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: path))
        }
        if let poster = item.posterFile {
            try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: poster))
        }
        if let package = item.packageId, item.serverInstanceId == settings.instanceId,
           item.userId == settings.userId {
            try? await PlurxAPI(origin: settings.origin).deleteOfflinePackage(package)
        }
        _ = try? await catalog.remove(id: item.id)
        await refresh()
    }

    func removeOtherProfile(_ profile: OfflineProfileSummary) async {
        let removed = (try? await catalog.removeProfile(
            serverInstanceId: profile.serverInstanceId,
            userId: profile.userId
        )) ?? []
        for item in removed {
            if let path = item.localAssetRelativePath {
                try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: path))
            }
            if let poster = item.posterFile {
                try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: poster))
            }
        }
        await refresh()
    }

    func hasCurrentProfileDownloads() async -> Bool {
        guard let instance = settings.instanceId, let user = settings.userId else { return false }
        return !(await catalog.profileItems(serverInstanceId: instance, userId: user)).isEmpty
    }

    func removeCurrentProfile() async {
        guard let instance = settings.instanceId, let user = settings.userId else { return }
        let current = await catalog.profileItems(serverInstanceId: instance, userId: user)
        for item in current { await remove(item) }
    }

    func recordProgress(id: String, positionMs: Int, durationMs: Int?) async {
        guard var item = await catalog.item(id: id) else { return }
        item.positionMs = max(0, positionMs)
        item.durationMs = durationMs ?? item.durationMs
        item.recordedAt = Date()
        item.pendingProgress = true
        item.updatedAt = Date()
        try? await catalog.replace(item)
        await refresh()
    }

    func handleEvents(forBackgroundURLSession identifier: String, completion: @escaping () -> Void) {
        stateLock.lock()
        backgroundCompletions[identifier] = completion
        stateLock.unlock()
    }

    private func awaitPreparation(localId: String, api: PlurxAPI) async throws {
        guard await preparations.begin(localId) else { return }
        do {
            try await pollPreparation(localId: localId, api: api)
            await preparations.finish(localId)
        } catch {
            await preparations.finish(localId)
            throw error
        }
    }

    private func pollPreparation(localId: String, api: PlurxAPI) async throws {
        guard var local = await catalog.item(id: localId), let packageId = local.packageId else {
            return
        }
        while true {
            let package = try await api.offlinePackage(packageId)
            local.actualHeight = package.output.height
            local.durationMs = package.durationMs ?? local.durationMs
            local.bytesTotal = package.actualBytes ?? package.estimatedBytes
            local.phase = package.phase
            local.updatedAt = Date()
            switch package.state {
            case "ready":
                local.state = .readyToTransfer
                try await catalog.replace(local)
                await refresh()
                try await startTransfer(local: local, api: api)
                return
            case "failed":
                local.state = .failed
                local.errorMessage = package.error?.message ?? "The server could not prepare this download."
                try await catalog.replace(local)
                await refresh()
                return
            default:
                local.state = package.state == "queued" ? .queued : .preparing
                try await catalog.replace(local)
                await refresh()
                try await Task.sleep(for: .seconds(2))
            }
        }
    }

    private func startTransfer(local: OfflineItem, api: PlurxAPI) async throws {
        for session in [wifiSession!, anyNetworkSession!] {
            let tasks = await allTasks(in: session)
            if let task = tasks.first(where: { $0.taskDescription == local.id }) {
                task.resume()
                var existing = local
                existing.state = .downloading
                existing.phase = "downloading"
                existing.errorMessage = nil
                existing.updatedAt = Date()
                try await catalog.replace(existing)
                await refresh()
                return
            }
        }
        // Another foreground trigger may have updated or removed this row
        // while the task lookup was in flight. The durable row owns the stable
        // token decision, so re-read it immediately before binding a lease.
        guard var item = await catalog.item(id: local.id),
              let packageId = item.packageId
        else { return }
        let token = item.leaseToken ?? Self.randomToken()
        item.leaseToken = token
        item.updatedAt = Date()
        // Persist the token before asking the server to bind it. A crash after
        // the PUT must retry the identical capability, never rotate the URL.
        try await catalog.replace(item)
        let lease = try await api.putOfflineLease(packageId: packageId, token: token)
        guard let manifest = api.absoluteOfflineManifest(lease.manifestUrl) else {
            throw APIError.badURL
        }
        item.manifestURL = manifest.absoluteString
        item.bytesTotal = lease.bytes
        item.durationMs = lease.durationMs
        item.updatedAt = Date()
        try await catalog.replace(item)

        if let available = try? URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
            .resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey])
            .volumeAvailableCapacityForImportantUsage {
            let required = Int64(Double(lease.bytes) * 1.10) + 256 * 1_024 * 1_024
            guard available >= required else {
                throw APIError.transport("Not enough device storage for this download")
            }
        }

        let asset = AVURLAsset(url: manifest)
        let preferred = try await asset.load(.preferredMediaSelection)
        guard let selection = preferred.mutableCopy() as? AVMutableMediaSelection else {
            throw APIError.transport("The download media selection could not be created")
        }
        if item.subtitleIndex != nil {
            guard let group = try await asset.loadMediaSelectionGroup(for: .legible),
                  let option = group.options.first
            else {
                throw APIError.transport("The selected offline subtitle is unavailable")
            }
            // The server advertises exactly the requested rendition. Select it
            // explicitly; the platform's preferred selection may choose none.
            selection.select(option, in: group)
        }
        let configuration = AVAssetDownloadConfiguration(asset: asset, title: item.title)
        configuration.primaryContentConfiguration.mediaSelections = [selection]
        configuration.auxiliaryContentConfigurations = []
        let session = settings.offlineNetwork == .wifiOnly ? wifiSession! : anyNetworkSession!
        let task = session.makeAssetDownloadTask(downloadConfiguration: configuration)
        task.taskDescription = item.id
        item.state = .downloading
        item.phase = settings.offlineNetwork == .wifiOnly ? "downloading_wifi" : "downloading"
        item.errorMessage = nil
        item.updatedAt = Date()
        try await catalog.replace(item)
        await refresh()
        task.resume()
    }

    private func syncPendingProgress(api: PlurxAPI) async {
        guard let instance = settings.instanceId, let user = settings.userId else { return }
        let pending = await catalog.newestPendingProgress(serverInstanceId: instance, userId: user)
        for var item in pending {
            let at = item.recordedAt.map { Int($0.timeIntervalSince1970) }
            do {
                try await api.progress(
                    itemId: item.itemId,
                    positionMs: item.positionMs,
                    durationMs: item.durationMs,
                    recordedAt: at
                )
                item.pendingProgress = false
                try await catalog.replace(item)
            } catch {
                break
            }
        }
        await refresh()
    }

    private func makeSession(suffix: String, anyNetwork: Bool) -> AVAssetDownloadURLSession {
        let bundle = Bundle.main.bundleIdentifier ?? "tv.plurx.app"
        let configuration = URLSessionConfiguration.background(
            withIdentifier: "\(bundle).offline.\(suffix)"
        )
        configuration.sessionSendsLaunchEvents = true
        configuration.isDiscretionary = false
        configuration.allowsCellularAccess = anyNetwork
        configuration.allowsExpensiveNetworkAccess = anyNetwork
        configuration.allowsConstrainedNetworkAccess = anyNetwork
        return AVAssetDownloadURLSession(
            configuration: configuration,
            assetDownloadDelegate: self,
            delegateQueue: delegateQueue
        )
    }

    private func restoreTasks() async {
        let wifiTasks = await allTasks(in: wifiSession!)
        let anyNetworkTasks = await allTasks(in: anyNetworkSession!)
        let tasks = wifiTasks + anyNetworkTasks
        let activeIds = Set(tasks.compactMap(\.taskDescription))
        for task in tasks {
            guard let id = task.taskDescription,
                  var item = await catalog.item(id: id),
                  item.state != .downloaded
            else { continue }
            item.state = task.state == .suspended ? .paused : .downloading
            item.phase = task.state == .suspended ? "paused" : item.phase
            item.updatedAt = Date()
            try? await catalog.replace(item)
        }
        // A persisted downloading row with no background task is interrupted,
        // not missing media and not still moving. Foreground catch-up can then
        // resume it through the ordinary preparation path.
        for var item in await catalog.allItems()
            where item.state == .downloading && !activeIds.contains(item.id) {
            item.state = .paused
            item.phase = "paused"
            item.errorMessage = "Download interrupted — tap Resume"
            item.updatedAt = Date()
            try? await catalog.replace(item)
        }
        await refresh()
    }

    private func allTasks(in session: AVAssetDownloadURLSession) async -> [URLSessionTask] {
        await withCheckedContinuation { continuation in
            session.getAllTasks { continuation.resume(returning: $0) }
        }
    }

    private static func randomToken() -> String {
        var bytes = [UInt8](repeating: 0, count: 32)
        let status = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        precondition(status == errSecSuccess, "secure random generation failed")
        return bytes.map { String(format: "%02x", $0) }.joined()
    }

    private func cachePoster(path: String?, id: String) async -> String? {
        guard let path, let url = Session.shared.url(path) else { return nil }
        var request = URLRequest(url: url)
        Session.shared.authorize(&request)
        guard let (data, response) = try? await URLSession.shared.data(for: request),
              (response as? HTTPURLResponse).map({ (200..<300).contains($0.statusCode) }) != false,
              !data.isEmpty
        else { return nil }
        let directory = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        )[0].appendingPathComponent("Offline/artwork", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let target = directory.appendingPathComponent("\(id).image")
        guard (try? data.write(to: target, options: .atomic)) != nil else { return nil }
        return OfflineCatalog.relativeLocalPath(for: target)
    }

    private static func trackLabel(language: String?, title: String?) -> String? {
        title?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
            ?? language?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
    }

    private func rememberLocation(_ location: URL, for task: AVAssetDownloadTask) {
        guard let id = task.taskDescription,
              let relative = OfflineCatalog.relativeLocalPath(for: location)
        else { return }
        var resourceValues = URLResourceValues()
        resourceValues.isExcludedFromBackup = true
        var protectedLocation = location
        try? protectedLocation.setResourceValues(resourceValues)
        stateLock.lock()
        let priorWrite = taskLocationWrites[task.taskIdentifier]
        let write = Task { [weak self] in
            await priorWrite?.value
            guard let self else { return }
            _ = try? await self.catalog.update(id: id) { item in
                item.localAssetRelativePath = relative
                return true
            }
        }
        taskLocations[task.taskIdentifier] = location
        taskLocationWrites[task.taskIdentifier] = write
        stateLock.unlock()
    }
}

extension OfflineDownloadManager: AVAssetDownloadDelegate {
    func urlSession(
        _ session: URLSession,
        assetDownloadTask: AVAssetDownloadTask,
        didLoad timeRange: CMTimeRange,
        totalTimeRangesLoaded loadedTimeRanges: [NSValue],
        timeRangeExpectedToLoad: CMTimeRange
    ) {
        guard let id = assetDownloadTask.taskDescription else { return }
        let expected = timeRangeExpectedToLoad.duration.seconds
        let loaded = loadedTimeRanges.reduce(0.0) { result, value in
            result + value.timeRangeValue.duration.seconds
        }
        guard expected.isFinite, expected > 0 else { return }
        let fraction = min(1, max(0, loaded / expected))
        Task {
            // A didLoad callback can be queued behind didComplete. Consult
            // both the system task and the actor-owned terminal state before
            // accepting it; either one is enough to prove this write is late.
            guard assetDownloadTask.state == .running else { return }
            let changed = try? await catalog.update(id: id) { item in
                guard item.state == .downloading else { return false }
                if let total = item.bytesTotal {
                    item.bytesDownloaded = Int64(Double(total) * fraction)
                }
                return true
            }
            if changed != nil { await refresh() }
        }
    }

    func urlSession(
        _ session: URLSession,
        assetDownloadTask: AVAssetDownloadTask,
        didFinishDownloadingTo location: URL
    ) {
        rememberLocation(location, for: assetDownloadTask)
    }

    @available(iOS 18.0, *)
    func urlSession(
        _ session: URLSession,
        assetDownloadTask: AVAssetDownloadTask,
        willDownloadTo location: URL
    ) {
        rememberLocation(location, for: assetDownloadTask)
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let id = task.taskDescription else { return }
        stateLock.lock()
        let location = taskLocations.removeValue(forKey: task.taskIdentifier)
        let locationWrite = taskLocationWrites.removeValue(forKey: task.taskIdentifier)
        stateLock.unlock()
        Task {
            await locationWrite?.value
            guard let snapshot = await catalog.item(id: id) else { return }
            if let error {
                let path = location.flatMap(OfflineCatalog.relativeLocalPath(for:))
                    ?? snapshot.localAssetRelativePath
                if let path {
                    try? FileManager.default.removeItem(at: OfflineCatalog.localURL(for: path))
                }
                // Classified before the catalog closure so the message it
                // stores is a plain string, not a captured error.
                let message: String? = (error as NSError).code == NSURLErrorCancelled
                    ? "Paused — tap Resume"
                    : Connectivity.message(for: error, server: settings.origin)
                _ = try? await catalog.update(id: id) { item in
                    item.state = .paused
                    item.phase = "paused"
                    item.errorMessage = message
                    item.localAssetRelativePath = nil
                    return true
                }
            } else {
                let path = location.flatMap(OfflineCatalog.relativeLocalPath(for:))
                    ?? snapshot.localAssetRelativePath
                guard let path else {
                    _ = try? await catalog.update(id: id) { item in
                        item.state = .failed
                        item.errorMessage = "The completed download location was not saved."
                        return true
                    }
                    await refresh()
                    return
                }
                let localURL = OfflineCatalog.localURL(for: path)
                let asset = AVURLAsset(url: localURL)
                let duration = try? await asset.load(.duration)
                let subtitleReady: Bool
                if snapshot.subtitleIndex != nil {
                    if let group = try? await asset.loadMediaSelectionGroup(for: .legible),
                       let cache = asset.assetCache {
                        subtitleReady = !cache.mediaSelectionOptions(in: group).isEmpty
                    } else {
                        subtitleReady = false
                    }
                } else {
                    subtitleReady = true
                }
                if asset.assetCache?.isPlayableOffline == true,
                   let duration, duration.seconds.isFinite, duration.seconds > 0,
                   subtitleReady {
                    let policy = AVMutableAssetDownloadStorageManagementPolicy()
                    policy.priority = .important
                    policy.expirationDate = Date().addingTimeInterval(10 * 365 * 24 * 60 * 60)
                    AVAssetDownloadStorageManager.shared().setStorageManagementPolicy(
                        policy,
                        for: localURL
                    )
                    let completed = try? await catalog.update(id: id) { item in
                        item.localAssetRelativePath = path
                        item.state = .downloaded
                        item.phase = "downloaded"
                        item.bytesDownloaded = item.bytesTotal ?? item.bytesDownloaded
                        item.errorMessage = nil
                        return true
                    }
                    if let package = completed?.packageId {
                        try? await PlurxAPI(origin: settings.origin).completeOfflinePackage(package)
                    }
                } else {
                    try? FileManager.default.removeItem(at: localURL)
                    _ = try? await catalog.update(id: id) { item in
                        item.localAssetRelativePath = nil
                        item.state = .failed
                        item.errorMessage = "The package is not complete enough for offline playback."
                        return true
                    }
                }
            }
            await refresh()
        }
    }

    func urlSessionDidFinishEvents(forBackgroundURLSession session: URLSession) {
        guard let identifier = session.configuration.identifier else { return }
        stateLock.lock()
        let completion = backgroundCompletions.removeValue(forKey: identifier)
        stateLock.unlock()
        DispatchQueue.main.async { completion?() }
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
#endif
