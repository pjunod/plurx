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
        // A cache hit is drawn without ever clearing what is on screen, so
        // scrolling a grid back over cells it has already shown does not flash
        // the placeholder.
        if let cached = AuthImageCache.shared.cached(key) {
            image = cached
            return
        }
        image = nil
        let loaded = await AuthImageCache.shared.image(
            path: path,
            maxPixelSize: maxPixelSize,
            key: key
        )
        guard !Task.isCancelled else { return }
        image = loaded
    }
}

/// Fetch, downsample, and remember artwork.
///
/// Deliberately a plain final class rather than an actor: every method here is
/// either thread-safe already (`NSCache`, `URLSession`) or a pure function of
/// its arguments, and being nonisolated is what keeps the ImageIO decode off
/// the main actor. `AuthImage.load()` is `@MainActor` and awaits into this;
/// the suspension is the hop.
final class AuthImageCache: @unchecked Sendable {
    static let shared = AuthImageCache()

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
    private let cache = NSCache<NSString, UIImage>()
    /// Artwork gets the same connectivity treatment as the API: the request
    /// that races the iOS local-network prompt waits for the answer instead of
    /// failing under it and leaving a permanent grey rectangle.
    private let session: URLSession

    private init() {
        cache.countLimit = 300
        let configuration = URLSessionConfiguration.default
        configuration.waitsForConnectivity = true
        configuration.timeoutIntervalForRequest = 30
        configuration.timeoutIntervalForResource = 60
        session = URLSession(configuration: configuration)
    }

    /// Origin and pixel ceiling are both part of the identity: the same path on
    /// another server is another picture, and the same picture at poster size
    /// is not the one the backdrop wants.
    static func key(origin: String, path: String, maxPixelSize: CGFloat) -> String {
        "\(origin)\(path)|\(Int(maxPixelSize.rounded()))"
    }

    func cached(_ key: String) -> UIImage? {
        cache.object(forKey: key as NSString)
    }

    func clear() {
        cache.removeAllObjects()
    }

    /// One retry, because the first attempt is the one most likely to have
    /// raced a permission sheet or a Wi-Fi wake. Before this, a single failed
    /// poster stayed a placeholder for the life of the process — `.task(id:)`
    /// only re-runs when the path changes, and the path never does.
    func image(path: String, maxPixelSize: CGFloat, key: String) async -> UIImage? {
        if let hit = cached(key) { return hit }
        guard let url = Session.shared.url(path) else { return nil }
        var request = URLRequest(url: url)
        Session.shared.authorize(&request)

        for _ in 0..<2 {
            if Task.isCancelled { return nil }
            guard let result = try? await session.data(for: request) else {
                continue                     // transport failure: one more try
            }
            let (data, response) = result
            if let http = response as? HTTPURLResponse,
               !(200..<300).contains(http.statusCode) {
                return nil                   // no artwork, or no longer authorized
            }
            guard let decoded = Self.downsample(data, maxPixelSize: maxPixelSize) else {
                return nil
            }
            cache.setObject(decoded, forKey: key as NSString)
            return decoded
        }
        return nil
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
