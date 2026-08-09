import AVFoundation
import Foundation
import QuartzCore
import UIKit

enum PGSOverlayManifestFetch {
    case preparing(retryAfterMs: Int)
    case ready(PGSOverlayManifest)
}

enum PGSOverlayManifestDisposition: Equatable {
    case ready
    case preparing
    case terminal
}

struct PGSOverlayPreparing: Codable {
    let state: String
    let retryAfterMs: Int
}

struct PGSOverlayManifest: Codable, Equatable, Sendable {
    let schema: Int
    let generation: String
    let fileId: Int
    let trackIndex: Int
    let kind: String
    let timebase: String
    let durationMs: Int
    let cues: [PGSOverlayCue]

    func validated(fileId expectedFileId: Int, trackIndex expectedTrackIndex: Int) throws -> Self {
        guard schema == 1,
              kind == "pgs",
              timebase == "source_ms",
              fileId == expectedFileId,
              trackIndex == expectedTrackIndex,
              durationMs > 0,
              Self.isSHA256(generation),
              cues.count <= PGSOverlayPolicy.maximumManifestCues
        else { throw PGSOverlayError.invalidManifest }

        var previousEnd = 0
        var imageDimensions: [String: (width: Int, height: Int)] = [:]
        for cue in cues {
            guard !cue.id.isEmpty,
                  cue.startMs >= previousEnd,
                  cue.endMs > cue.startMs,
                  cue.endMs <= durationMs,
                  (1...PGSOverlayPolicy.maximumCanvasWidth).contains(cue.canvasWidth),
                  (1...PGSOverlayPolicy.maximumCanvasHeight).contains(cue.canvasHeight),
                  cue.objects.count <= PGSOverlayPolicy.maximumObjectsPerCue
            else { throw PGSOverlayError.invalidManifest }
            previousEnd = cue.endMs

            for object in cue.objects {
                guard object.x >= 0,
                      object.y >= 0,
                      object.width > 0,
                      object.height > 0,
                      object.x <= cue.canvasWidth,
                      object.y <= cue.canvasHeight,
                      object.width <= cue.canvasWidth - object.x,
                      object.height <= cue.canvasHeight - object.y,
                      Self.objectHash(
                        from: object.image,
                        generation: generation
                      ) != nil
                else { throw PGSOverlayError.invalidManifest }
                if let existing = imageDimensions[object.image] {
                    guard existing.width == object.width,
                          existing.height == object.height
                    else { throw PGSOverlayError.invalidManifest }
                } else {
                    imageDimensions[object.image] = (object.width, object.height)
                }
            }
        }
        return self
    }

    static func objectHash(from path: String, generation: String) -> String? {
        let prefix = "overlay/\(generation)/objects/"
        guard path.hasPrefix(prefix), path.hasSuffix(".png") else { return nil }
        let hash = String(path.dropFirst(prefix.count).dropLast(4))
        return isSHA256(hash) ? hash : nil
    }

    static func isSHA256(_ value: String) -> Bool {
        value.count == 64 && value.utf8.allSatisfy {
            (48...57).contains($0) || (97...102).contains($0)
        }
    }
}

struct PGSOverlayCue: Codable, Equatable, Sendable {
    let id: String
    let startMs: Int
    let endMs: Int
    let canvasWidth: Int
    let canvasHeight: Int
    let objects: [PGSOverlayObject]
}

struct PGSOverlayObject: Codable, Equatable, Sendable {
    let image: String
    let x: Int
    let y: Int
    let width: Int
    let height: Int
}

enum PGSOverlayError: LocalizedError, Equatable {
    case invalidManifest
    case invalidImage
    case memoryLimit
    case preparationTimedOut

    var errorDescription: String? {
        switch self {
        case .invalidManifest:
            return "The server returned an invalid PGS overlay manifest."
        case .invalidImage:
            return "The server returned an invalid PGS subtitle image."
        case .memoryLimit:
            return "The PGS subtitle window exceeded the device memory limit."
        case .preparationTimedOut:
            return "PGS subtitles took too long to prepare."
        }
    }
}

enum PGSOverlayStatus: Equatable {
    case off
    case preparing
    case ready
    case failed(String)

    var label: String? {
        switch self {
        case .off: return nil
        case .preparing: return "PGS overlay · preparing"
        case .ready: return "PGS overlay"
        case .failed: return "PGS overlay · unavailable"
        }
    }
}

enum PGSOverlayPolicy {
    static let protocolName = "pgs-v1"
    static let maximumCanvasWidth = 4_096
    static let maximumCanvasHeight = 2_160
    static let maximumObjectsPerCue = 64
    static let maximumManifestCues = 250_000
    static let maximumScheduledCues = 2_048
    static let decodedImageBudgetBytes = 96 * 1_024 * 1_024
    static let lookBehindMs = 5_000
    static let lookAheadMs = 90_000
    static let refreshMarginMs = 20_000
    static let maximumPrepareSeconds = 600

    static func manifestDisposition(_ statusCode: Int) -> PGSOverlayManifestDisposition {
        switch statusCode {
        case 200: .ready
        case 202, 503: .preparing
        default: .terminal
        }
    }

    static func retryAfterMs(_ header: String?) -> Int {
        min(max(250, (Int(header ?? "") ?? 1) * 1_000), 5_000)
    }

    static func periodicRefreshPosition(currentMs: Int, overlayIsActive: Bool) -> Int? {
        overlayIsActive ? currentMs : nil
    }

    static func itemTimeMs(sourceTimeMs: Int, baseMs: Int) -> Int {
        sourceTimeMs - baseMs
    }

    static func windowRange(at sourceTimeMs: Int, durationMs: Int) -> Range<Int> {
        let lower = max(0, sourceTimeMs - lookBehindMs)
        let upper = min(durationMs, max(lower + 1, sourceTimeMs + lookAheadMs))
        return lower..<upper
    }

    static func shouldRefresh(
        sourceTimeMs: Int,
        loadedRange: Range<Int>?
    ) -> Bool {
        guard let loadedRange else { return true }
        return sourceTimeMs < loadedRange.lowerBound
            || sourceTimeMs >= loadedRange.upperBound - refreshMarginMs
    }

    static func windowFitsDecodedBudget(_ cues: [PGSOverlayCue]) -> Bool {
        var paths: Set<String> = []
        var bytes = 0
        for object in cues.lazy.flatMap(\.objects) where paths.insert(object.image).inserted {
            let (pixels, pixelOverflow) = object.width.multipliedReportingOverflow(
                by: object.height
            )
            let (imageBytes, byteOverflow) = pixels.multipliedReportingOverflow(by: 4)
            guard !pixelOverflow,
                  !byteOverflow,
                  imageBytes <= decodedImageBudgetBytes - bytes
            else { return false }
            bytes += imageBytes
        }
        return true
    }

    static func objectFrame(
        _ object: PGSOverlayObject,
        canvasWidth: Int,
        canvasHeight: Int,
        destination: CGRect
    ) -> CGRect {
        guard canvasWidth > 0, canvasHeight > 0 else { return .zero }
        let scale = min(
            destination.width / CGFloat(canvasWidth),
            destination.height / CGFloat(canvasHeight)
        )
        let originX = destination.minX
            + (destination.width - CGFloat(canvasWidth) * scale) / 2
        let originY = destination.minY
            + (destination.height - CGFloat(canvasHeight) * scale) / 2
        return CGRect(
            x: originX + CGFloat(object.x) * scale,
            y: originY + CGFloat(object.y) * scale,
            width: CGFloat(object.width) * scale,
            height: CGFloat(object.height) * scale
        )
    }
}

struct PGSOverlayRenderableObject {
    let object: PGSOverlayObject
    let image: CGImage
}

struct PGSOverlayRenderableCue {
    let cue: PGSOverlayCue
    let objects: [PGSOverlayRenderableObject]
}

/// One bounded scheduling window. Images are decoded once and retained only
/// while this or the small controller-owned LRU needs them.
struct PGSOverlayWindow {
    let revision: Int
    let generation: String
    let baseMs: Int
    let sourceRange: Range<Int>
    let cues: [PGSOverlayRenderableCue]
}
