import Foundation

enum OfflineQuality: String, Codable, CaseIterable, Identifiable {
    case standard
    case high

    var id: String { rawValue }
    var maximumHeight: Int { self == .standard ? 720 : 1080 }
    var label: String { self == .standard ? "Standard" : "High" }
}

enum OfflineNetworkPolicy: String, Codable, CaseIterable, Identifiable {
    case wifiOnly
    case anyNetwork

    var id: String { rawValue }
    var label: String { self == .wifiOnly ? "Wi-Fi only" : "Any network" }
}

enum OfflineState: String, Codable {
    case intent
    case queued
    case preparing
    case readyToTransfer
    case downloading
    case downloaded
    case paused
    case failed
    case missing
}

struct OfflineItem: Codable, Identifiable, Equatable {
    var id: String
    var requestId: String
    var serverInstanceId: String
    var userId: Int
    var itemId: Int
    var fileId: Int
    var packageId: String?
    /// Client-generated transfer capability. It is not the login bearer and
    /// is stable for the life of one package so AVFoundation's URL never
    /// changes under a restored background task.
    var leaseToken: String?
    var manifestURL: String?
    var title: String
    var context: String?
    var durationMs: Int?
    var posterFile: String?
    var requestedHeight: Int
    var actualHeight: Int?
    var audioLabel: String?
    var subtitleLabel: String?
    var state: OfflineState
    var phase: String?
    var bytesDownloaded: Int64
    var bytesTotal: Int64?
    var localAssetRelativePath: String?
    var markers: [Marker]
    var positionMs: Int
    var recordedAt: Date?
    var pendingProgress: Bool
    var errorMessage: String?
    var updatedAt: Date

    var isPlayable: Bool { state == .downloaded && localAssetRelativePath != nil }
}

struct OfflineQualityOption: Codable, Identifiable, Equatable {
    var id: Int { height }
    let height: Int
    let label: String
    let estimatedBytes: Int64
    let reservedBytes: Int64
}

struct OfflineAudioOption: Codable, Identifiable, Equatable {
    var id: Int { index }
    let index: Int
    let codec: String
    var channels: Int?
    var language: String?
    var title: String?
    var `default`: Bool
}

struct OfflineSubtitleOption: Codable, Identifiable, Equatable {
    var id: Int { index }
    let index: Int
    let codec: String
    var language: String?
    var title: String?
    var `default`: Bool
    var forced: Bool
    let offlineMode: String
}

struct OfflineOptions: Codable, Equatable {
    let fileId: Int
    let qualities: [OfflineQualityOption]
    let audio: [OfflineAudioOption]
    let subtitles: [OfflineSubtitleOption]
    var recommendedAudioIndex: Int?
    var recommendedSubtitleIndex: Int?
}

struct CreateOfflinePackageRequest: Codable {
    let requestId: String
    let height: Int
    var audioIndex: Int?
    var subtitleIndex: Int?
}

struct OfflinePackageOutput: Codable, Equatable {
    let height: Int
    let videoCodec: String
    let audioCodec: String
    let dynamicRange: String
    let subtitleMode: String
}

struct OfflinePackageFailure: Codable, Equatable {
    let code: String
    let message: String
}

struct OfflinePackageStatus: Codable, Equatable {
    let id: String
    let state: String
    let phase: String
    let statusUrl: String
    var progress: Double?
    let bytesReady: Int64
    let estimatedBytes: Int64
    var actualBytes: Int64?
    var durationMs: Int?
    let output: OfflinePackageOutput
    var error: OfflinePackageFailure?
}

struct OfflineLeaseRequest: Codable {
    let token: String
}

struct OfflineLeaseResponse: Codable, Equatable {
    let manifestUrl: String
    let expiresAt: Int64
    let bytes: Int64
    let durationMs: Int
}
