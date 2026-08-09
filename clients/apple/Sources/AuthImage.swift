import CryptoKit
import ImageIO
import SwiftUI
import UIKit

/// Async poster/backdrop image. Loads `/api/v1/images/…` with the bearer header,
/// decoded straight down to the size of the cell that will draw it, showing a
/// muted placeholder until then.
///
/// Pass `targetSize` wherever the layout already knows its frame — the whole
/// point is to hand ImageIO a pixel ceiling instead of decoding a 2000×3000
/// poster into a 132-point cell, which is what made large tvOS grids stutter.
struct AuthImage: View {
    let path: String?
    var contentMode: ContentMode = .fill
    /// The frame this image will be drawn into, in points. `nil` means "as
    /// large as this platform's screen" — the honest answer for a full-bleed
    /// backdrop, which really does want most of the panel's pixels.
    var targetSize: CGSize? = nil
    @State private var image: UIImage?

    var body: some View {
        ZStack {
            if let image {
                Image(uiImage: image)
                    .resizable()
                    .aspectRatio(contentMode: contentMode)
            } else {
                Palette.surfaceHi
            }
        }
        .task(id: path) { await load() }
    }

    /// The longest edge to decode, in pixels.
    private var maxPixelSize: CGFloat {
        let points = targetSize.map { max($0.width, $0.height) }
            ?? AuthImageCache.fullBleedPoints
        return max(1, points.rounded(.up) * AuthImageCache.displayScale)
    }

    @MainActor
    private func load() async {
        guard let path, !path.isEmpty else {
            image = nil
            return
        }
        let key = AuthImageCache.key(origin: Session.shared.origin, path: path,
                                     maxPixelSize: maxPixelSize)
        // Read memory and disk before touching the network. An expired disk
        // entry is still painted first, then refreshed in place below: launch
        // and scrolling never have to wait behind Wi-Fi for artwork we have.
        let local = await AuthImageCache.shared.localImage(
            path: path,
            maxPixelSize: maxPixelSize,
            key: key
        )
        guard !Task.isCancelled else { return }
        if let local {
            image = local.image
            if local.isFresh { return }
        } else {
            image = nil
        }

        let refreshed = await AuthImageCache.shared.refreshImage(
            path: path,
            maxPixelSize: maxPixelSize,
            key: key
        )
        guard !Task.isCancelled else { return }
        // A transient refresh failure leaves the stale-but-useful disk image
        // visible. A later view will try the refresh again.
        if let refreshed { image = refreshed }
    }
}

struct AuthImageCacheHit {
    let image: UIImage
    let isFresh: Bool
}

private final class AuthImageMemoryEntry: NSObject {
    let image: UIImage
    let storedAt: Date

    init(image: UIImage, storedAt: Date) {
        self.image = image
        self.storedAt = storedAt
    }
}

/// Fetch, downsample, and remember artwork in memory and on disk.
///
/// Deliberately a plain final class rather than an actor: every method here is
/// either thread-safe already (`NSCache`, `URLSession`), delegates serialized
/// file work to `AuthImageDiskCache`, or is a pure function of its arguments.
/// Being nonisolated keeps the ImageIO decode off the main actor.
final class AuthImageCache: @unchecked Sendable {
    static let shared = AuthImageCache()

    /// Match the server's `Cache-Control: max-age=604800`. An older entry is
    /// shown immediately but refreshed in the background of the view task.
    static let freshAge: TimeInterval = 7 * 24 * 60 * 60
    /// Keep a stale fallback for offline launches, but not forever.
    static let maximumStaleAge: TimeInterval = 30 * 24 * 60 * 60
    /// Artwork is an opportunistic cache, not user data. LRU pruning after
    /// each write keeps it below 256 MiB across app launches.
    static let diskByteLimit: Int64 = 256 * 1_024 * 1_024

    /// Rounded up to the densest panel each platform ships rather than asked
    /// per-trait: the decode runs off the main actor, where the current trait
    /// collection is not ours to read. Over-decoding a little costs memory that
    /// the size ceiling has already bounded; under-decoding looks soft.
    #if os(tvOS)
    static let displayScale: CGFloat = 2
    static let fullBleedPoints: CGFloat = 1_920
    #else
    static let displayScale: CGFloat = 3
    static let fullBleedPoints: CGFloat = 1_366
    #endif

    /// Bounded by count, not bytes, because every entry is already downsampled
    /// to a cell: a few hundred posters at grid size is tens of megabytes, and
    /// `NSCache` evicts under memory pressure regardless.
    private let cache = NSCache<NSString, AuthImageMemoryEntry>()
    private let diskCache: AuthImageDiskCache
    private let downloads = AuthImageDownloadCoordinator()
    /// Artwork gets the same connectivity treatment as the API: the request
    /// that races the iOS local-network prompt waits for the answer instead of
    /// failing under it and leaving a permanent grey rectangle.
    private let session: URLSession

    private init() {
        let cacheRoot = FileManager.default.urls(
            for: .cachesDirectory,
            in: .userDomainMask
        ).first ?? FileManager.default.temporaryDirectory
        let diskCache = AuthImageDiskCache(
            directory: cacheRoot.appendingPathComponent("plurx-artwork-v1", isDirectory: true),
            byteLimit: Self.diskByteLimit,
            maximumStaleAge: Self.maximumStaleAge
        )
        self.diskCache = diskCache
        cache.countLimit = 300
        let configuration = URLSessionConfiguration.default
        configuration.waitsForConnectivity = true
        configuration.timeoutIntervalForRequest = 30
        configuration.timeoutIntervalForResource = 60
        // Raw artwork is owned by the bounded cache above. Avoid retaining a
        // second opaque URLCache copy of the same authenticated responses.
        configuration.urlCache = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: configuration)
        Task { await diskCache.prune() }
    }

    /// Origin and pixel ceiling are both part of the identity: the same path on
    /// another server is another picture, and the same picture at poster size
    /// is not the one the backdrop wants.
    static func key(origin: String, path: String, maxPixelSize: CGFloat) -> String {
        "\(sourceKey(origin: origin, path: path))|\(Int(maxPixelSize.rounded()))"
    }

    /// The original bytes are shared by every decode size, unlike the memory
    /// key. Absolute provider URLs intentionally ignore the selected server.
    static func sourceKey(origin: String, path: String) -> String {
        if let url = URL(string: path), url.scheme != nil {
            return url.absoluteString
        }
        return origin + path
    }

    static func isFresh(storedAt: Date, now: Date = Date()) -> Bool {
        now.timeIntervalSince(storedAt) <= freshAge
    }

    private func memoryHit(_ key: String, now: Date = Date()) -> AuthImageCacheHit? {
        guard let entry = cache.object(forKey: key as NSString) else { return nil }
        return AuthImageCacheHit(
            image: entry.image,
            isFresh: Self.isFresh(storedAt: entry.storedAt, now: now)
        )
    }

    /// Return a decoded memory/disk hit before the caller considers a network
    /// request. Disk entries keep their original bytes so one stored poster can
    /// be decoded appropriately for both a grid card and a full-screen hero.
    func localImage(path: String, maxPixelSize: CGFloat, key: String) async -> AuthImageCacheHit? {
        if let hit = memoryHit(key) { return hit }
        let source = Self.sourceKey(origin: Session.shared.origin, path: path)
        guard let entry = await diskCache.entry(for: source) else { return nil }
        guard let decoded = Self.downsample(entry.data, maxPixelSize: maxPixelSize) else {
            await diskCache.remove(source)
            return nil
        }
        cache.setObject(
            AuthImageMemoryEntry(image: decoded, storedAt: entry.storedAt),
            forKey: key as NSString
        )
        return AuthImageCacheHit(
            image: decoded,
            isFresh: Self.isFresh(storedAt: entry.storedAt)
        )
    }

    func clear() {
        cache.removeAllObjects()
        let diskCache = diskCache
        Task { await diskCache.removeAll() }
    }

    /// One retry, because the first attempt is the one most likely to have
    /// raced a permission sheet or a Wi-Fi wake. Before this, a single failed
    /// poster stayed a placeholder for the life of the process — `.task(id:)`
    /// only re-runs when the path changes, and the path never does.
    func refreshImage(path: String, maxPixelSize: CGFloat, key: String) async -> UIImage? {
        guard let url = Session.shared.url(path) else { return nil }
        var authorizedRequest = URLRequest(url: url)
        Session.shared.authorize(&authorizedRequest)
        let request = authorizedRequest
        let source = Self.sourceKey(origin: Session.shared.origin, path: path)
        let session = session
        let downloaded = await downloads.download(source) {
            for _ in 0..<2 {
                guard let result = try? await session.data(for: request) else {
                    continue                 // transport failure: one more retry
                }
                let (data, response) = result
                if let http = response as? HTTPURLResponse,
                   !(200..<300).contains(http.statusCode) {
                    return .terminalFailure  // gone, or no longer authorized
                }
                return .success(data)
            }
            return .transientFailure
        }

        switch downloaded {
        case let .success(data):
            guard let decoded = Self.downsample(data, maxPixelSize: maxPixelSize) else {
                cache.removeObject(forKey: key as NSString)
                await diskCache.remove(source)
                return nil
            }
            let storedAt = Date()
            cache.setObject(
                AuthImageMemoryEntry(image: decoded, storedAt: storedAt),
                forKey: key as NSString
            )
            await diskCache.store(data, for: source, storedAt: storedAt)
            return decoded
        case .terminalFailure:
            cache.removeObject(forKey: key as NSString)
            await diskCache.remove(source)
            return nil
        case .transientFailure:
            // Keep the stale entry as an offline fallback.
            return nil
        }
    }

    /// Decode straight to the cell's pixel ceiling. `UIImage(data:)` decodes
    /// the full raster and only then scales it, so a 2000×3000 poster cost
    /// ~24 MB and a full decode *per cell* on a tvOS `.extraLarge` grid.
    /// `CGImageSourceCreateThumbnailAtIndex` never materializes the large one.
    ///
    /// Pure and static so the test target can drive it without a server.
    static func downsample(_ data: Data, maxPixelSize: CGFloat) -> UIImage? {
        let sourceOptions = [kCGImageSourceShouldCache: false] as CFDictionary
        guard let source = CGImageSourceCreateWithData(data as CFData, sourceOptions) else {
            return UIImage(data: data)
        }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            // Honor EXIF orientation while resizing rather than after it.
            kCGImageSourceCreateThumbnailWithTransform: true,
            // Decode now, on this cooperative thread, instead of lazily on the
            // main thread at first draw — which is the stutter being removed.
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: max(1, Int(maxPixelSize.rounded())),
        ]
        guard let thumbnail = CGImageSourceCreateThumbnailAtIndex(
            source, 0, options as CFDictionary
        ) else {
            return UIImage(data: data)
        }
        // Scale 1: every use site is `.resizable()` inside an explicit frame,
        // so the point size of the UIImage is never consulted.
        return UIImage(cgImage: thumbnail)
    }
}

enum AuthImageDownloadResult: Sendable {
    case success(Data)
    case transientFailure
    case terminalFailure
}

/// Coalesce simultaneous poster/backdrop requests for the same URL. A rail can
/// ask for one source at several decode sizes; only the bytes cross the LAN
/// more than once when the previous request has actually completed.
actor AuthImageDownloadCoordinator {
    private var inFlight: [String: Task<AuthImageDownloadResult, Never>] = [:]

    func download(
        _ key: String,
        operation: @escaping @Sendable () async -> AuthImageDownloadResult
    ) async -> AuthImageDownloadResult {
        if let task = inFlight[key] { return await task.value }
        let task = Task { await operation() }
        inFlight[key] = task
        let result = await task.value
        inFlight[key] = nil
        return result
    }
}

/// Small file-backed LRU for the original artwork bytes. Metadata is separate
/// so expiry/pruning never has to load every large image into memory.
actor AuthImageDiskCache {
    struct Entry: Sendable {
        let data: Data
        let storedAt: Date
    }

    private struct Metadata: Codable {
        let storedAt: Date
    }

    private struct Candidate {
        let token: String
        let byteCount: Int64
        let lastAccessedAt: Date
    }

    private let directory: URL
    private let byteLimit: Int64
    private let maximumStaleAge: TimeInterval
    private let fileManager: FileManager
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(
        directory: URL,
        byteLimit: Int64,
        maximumStaleAge: TimeInterval,
        fileManager: FileManager = .default
    ) {
        self.directory = directory
        self.byteLimit = byteLimit
        self.maximumStaleAge = maximumStaleAge
        self.fileManager = fileManager
        try? fileManager.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
    }

    func entry(for key: String, now: Date = Date()) -> Entry? {
        ensureDirectory()
        let token = Self.token(for: key)
        let metadataURL = url(for: token, extension: "json")
        let dataURL = url(for: token, extension: "image")
        guard
            let metadataData = try? Data(contentsOf: metadataURL),
            let metadata = try? decoder.decode(Metadata.self, from: metadataData)
        else {
            removeToken(token)
            return nil
        }
        guard now.timeIntervalSince(metadata.storedAt) <= maximumStaleAge,
              let data = try? Data(contentsOf: dataURL)
        else {
            removeToken(token)
            return nil
        }
        // The image file's modification time is the LRU access clock. The
        // immutable JSON timestamp remains the freshness/expiry clock.
        try? fileManager.setAttributes(
            [.modificationDate: now],
            ofItemAtPath: dataURL.path
        )
        return Entry(data: data, storedAt: metadata.storedAt)
    }

    func store(_ data: Data, for key: String, storedAt: Date = Date()) {
        ensureDirectory()
        let token = Self.token(for: key)
        let dataURL = url(for: token, extension: "image")
        let metadataURL = url(for: token, extension: "json")
        do {
            try data.write(to: dataURL, options: .atomic)
            let metadata = try encoder.encode(Metadata(storedAt: storedAt))
            try metadata.write(to: metadataURL, options: .atomic)
            try? fileManager.setAttributes(
                [.modificationDate: storedAt],
                ofItemAtPath: dataURL.path
            )
        } catch {
            removeToken(token)
            return
        }
        prune(now: storedAt)
    }

    func remove(_ key: String) {
        removeToken(Self.token(for: key))
    }

    func removeAll() {
        try? fileManager.removeItem(at: directory)
        ensureDirectory()
    }

    func prune(now: Date = Date()) {
        ensureDirectory()
        let keys: Set<URLResourceKey> = [
            .fileSizeKey,
            .contentModificationDateKey,
        ]
        guard let files = try? fileManager.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: Array(keys),
            options: [.skipsHiddenFiles]
        ) else { return }

        var candidates: [Candidate] = []
        var totalBytes: Int64 = 0
        let imageFiles = files.filter { $0.pathExtension == "image" }
        let imageTokens = Set(imageFiles.map { $0.deletingPathExtension().lastPathComponent })

        // Crash-safe housekeeping for a half-written pair.
        for metadataURL in files where metadataURL.pathExtension == "json" {
            let token = metadataURL.deletingPathExtension().lastPathComponent
            if !imageTokens.contains(token) { try? fileManager.removeItem(at: metadataURL) }
        }

        for imageURL in imageFiles {
            let token = imageURL.deletingPathExtension().lastPathComponent
            let metadataURL = url(for: token, extension: "json")
            guard
                let metadataData = try? Data(contentsOf: metadataURL),
                let metadata = try? decoder.decode(Metadata.self, from: metadataData),
                now.timeIntervalSince(metadata.storedAt) <= maximumStaleAge,
                let values = try? imageURL.resourceValues(forKeys: keys)
            else {
                removeToken(token)
                continue
            }
            let imageBytes = Int64(values.fileSize ?? 0)
            let metadataBytes = Int64(
                (try? metadataURL.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
            )
            let byteCount = imageBytes + metadataBytes
            totalBytes += byteCount
            candidates.append(
                Candidate(
                    token: token,
                    byteCount: byteCount,
                    lastAccessedAt: values.contentModificationDate ?? metadata.storedAt
                )
            )
        }

        guard totalBytes > byteLimit else { return }
        for candidate in candidates.sorted(by: { $0.lastAccessedAt < $1.lastAccessedAt }) {
            removeToken(candidate.token)
            totalBytes -= candidate.byteCount
            if totalBytes <= byteLimit { break }
        }
    }

    nonisolated static func token(for key: String) -> String {
        SHA256.hash(data: Data(key.utf8)).map { String(format: "%02x", $0) }.joined()
    }

    private func ensureDirectory() {
        try? fileManager.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
    }

    private func url(for token: String, extension pathExtension: String) -> URL {
        directory.appendingPathComponent(token).appendingPathExtension(pathExtension)
    }

    private func removeToken(_ token: String) {
        try? fileManager.removeItem(at: url(for: token, extension: "image"))
        try? fileManager.removeItem(at: url(for: token, extension: "json"))
    }
}
