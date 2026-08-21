#if os(iOS)
import Foundation

enum OfflineBookState: String, Codable {
    case intent
    case downloading
    case downloaded
    case failed
    case missing
}

struct OfflineBookPreferences: Codable, Equatable {
    var font = "publisher"
    var fontSize = 100.0
    var lineHeight = 1.55
    var margin = 7.0
    var theme = "light"
    var flow = "paginated"
}

struct OfflineBook: Codable, Identifiable, Equatable {
    let id: String
    let serverInstanceId: String
    let userId: Int
    let itemId: Int
    let fileId: Int
    var revision: ReadingRevision?
    var title: String
    var author: String?
    var originalFilename: String
    var coverRelativePath: String?
    var publication: PublicationManifest?
    var limits: PublicationLimits?
    var state: OfflineBookState
    var phase: String
    var bytesDownloaded: Int64
    var bytesTotal: Int64
    var localPublicationRelativePath: String?
    var locator: ReadingLocator?
    var progression: Double
    var completed: Bool
    var recordedAt: Int?
    var pendingProgress: Bool
    var preferences: OfflineBookPreferences
    var errorMessage: String?
    var updatedAt: Date

    var isPlayable: Bool {
        state == .downloaded && localPublicationRelativePath != nil
            && publication != nil && limits != nil && revision != nil
    }

    func isSameEdition(
        serverInstanceId: String,
        userId: Int,
        itemId: Int,
        fileId: Int,
        revision: ReadingRevision
    ) -> Bool {
        self.serverInstanceId == serverInstanceId && self.userId == userId
            && self.itemId == itemId && self.fileId == fileId && self.revision == revision
    }
}
#endif
