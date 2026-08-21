#if os(iOS)
import SwiftUI
import UIKit

struct DownloadsView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject private var downloads = OfflineDownloadManager.shared
    @ObservedObject private var bookDownloads = OfflineBookManager.shared
    @State private var playing: OfflineItem?
    @State private var reading: OfflineBook?
    @State private var profileToDelete: OfflineProfileSummary?

    var body: some View {
        Group {
            if downloads.items.isEmpty && bookDownloads.books.isEmpty && combinedOtherProfiles.isEmpty {
                ContentUnavailableView(
                    "No downloads yet",
                    systemImage: "arrow.down.circle",
                    description: Text("Tap Download on a book, movie, or episode to use it without a connection.")
                )
            } else {
                List {
                    if !bookDownloads.books.isEmpty {
                        Section("Books") {
                            ForEach(bookDownloads.books) { book in
                                OfflineBookRow(book: book) {
                                    if book.isPlayable { reading = book }
                                } remove: {
                                    Task { await bookDownloads.remove(book) }
                                }
                            }
                        }
                    }

                    if !downloads.items.isEmpty {
                        Section {
                            ForEach(downloads.items) { item in
                                DownloadRow(item: item) {
                                    if item.isPlayable {
                                        playing = item
                                    } else if item.state == .paused {
                                        Task { await downloads.resume(item) }
                                    }
                                } remove: {
                                    Task { await downloads.remove(item) }
                                }
                            }
                        } header: {
                            Text(storageSummary)
                        }
                    }

                    if !combinedOtherProfiles.isEmpty {
                        Section("Other profiles") {
                            ForEach(combinedOtherProfiles) { profile in
                                HStack {
                                    Image(systemName: "person.crop.circle.badge.clock")
                                    VStack(alignment: .leading, spacing: 3) {
                                        Text("Another Cinema profile")
                                        Text("\(profile.items) items · \(Self.bytes(profile.bytes))")
                                            .font(.caption)
                                            .foregroundStyle(.secondary)
                                    }
                                    Spacer()
                                    Button(role: .destructive) {
                                        profileToDelete = profile
                                    } label: {
                                        Image(systemName: "trash")
                                    }
                                    .buttonStyle(.borderless)
                                }
                            }
                        }
                    }
                }
                .scrollContentBackground(.hidden)
            }
        }
        .background(Palette.bg.ignoresSafeArea())
        .navigationTitle("Downloads")
        .task {
            await downloads.refresh()
            await bookDownloads.refresh()
            await downloads.resumePendingPreparation()
            await bookDownloads.syncPendingProgress()
        }
        .refreshable {
            await downloads.refresh()
            await bookDownloads.refresh()
            await downloads.resumePendingPreparation()
            await bookDownloads.syncPendingProgress()
        }
        .fullScreenCover(item: $playing) { item in
            PlayerView(
                itemId: item.itemId,
                fileId: item.fileId,
                startMs: item.positionMs,
                durationMs: item.durationMs ?? 0,
                title: item.title,
                subtitle: item.context,
                offlineItem: item
            )
            .id(item.id)
            .environmentObject(model)
        }
        .fullScreenCover(item: $reading) { book in
            OfflineBookReaderView(book: book)
        }
        .confirmationDialog(
            "Delete this profile's downloads?",
            isPresented: Binding(
                get: { profileToDelete != nil },
                set: { if !$0 { profileToDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Delete downloads", role: .destructive) {
                guard let profile = profileToDelete else { return }
                Task {
                    await downloads.removeOtherProfile(profile)
                    await bookDownloads.removeProfile(profile)
                }
                profileToDelete = nil
            }
            Button("Keep", role: .cancel) { profileToDelete = nil }
        }
    }

    private var storageSummary: String {
        let bytes = downloads.items.reduce(Int64(0)) {
            $0 + max(0, $1.bytesTotal ?? $1.bytesDownloaded)
        }
        return "On this device · \(Self.bytes(bytes))"
    }

    private var combinedOtherProfiles: [OfflineProfileSummary] {
        var values: [String: OfflineProfileSummary] = [:]
        for profile in downloads.otherProfiles + bookDownloads.otherProfiles {
            let current = values[profile.id]
            values[profile.id] = OfflineProfileSummary(
                serverInstanceId: profile.serverInstanceId,
                userId: profile.userId,
                items: (current?.items ?? 0) + profile.items,
                bytes: (current?.bytes ?? 0) + profile.bytes
            )
        }
        return values.values.sorted { $0.id < $1.id }
    }

    private static func bytes(_ value: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: value, countStyle: .file)
    }
}

private struct OfflineBookRow: View {
    let book: OfflineBook
    let read: () -> Void
    let remove: () -> Void

    var body: some View {
        HStack(spacing: 14) {
            if let cover = book.coverRelativePath,
               let image = UIImage(contentsOfFile: OfflineCatalog.localURL(for: cover).path) {
                Image(uiImage: image).resizable().scaledToFill()
                    .frame(width: 48, height: 72).clipShape(RoundedRectangle(cornerRadius: 7))
            } else {
                Image(systemName: book.isPlayable ? "book.fill" : stateIcon)
                    .frame(width: 48, height: 72)
                    .foregroundStyle(book.isPlayable ? Palette.accent : .secondary)
                    .background(Palette.surfaceHi, in: RoundedRectangle(cornerRadius: 7))
            }
            VStack(alignment: .leading, spacing: 5) {
                Text(book.title).font(.headline).foregroundStyle(Palette.onBg)
                if let author = book.author { Text(author).font(.subheadline).foregroundStyle(.secondary) }
                Text(stateLabel).font(.caption).foregroundStyle(.secondary)
                if book.state == .downloading, book.bytesTotal > 0 {
                    ProgressView(value: Double(book.bytesDownloaded), total: Double(book.bytesTotal))
                        .tint(Palette.accent)
                }
                if let error = book.errorMessage { Text(error).font(.caption).foregroundStyle(.red) }
            }
            Spacer()
            Button(role: .destructive, action: remove) { Image(systemName: "trash") }
                .buttonStyle(.borderless)
        }
        .contentShape(Rectangle()).onTapGesture(perform: read)
    }

    private var stateIcon: String {
        switch book.state {
        case .intent: return "clock"
        case .downloading: return "arrow.down"
        case .downloaded: return "book.fill"
        case .failed, .missing: return "exclamationmark.triangle"
        }
    }

    private var stateLabel: String {
        switch book.state {
        case .intent: return "Queued"
        case .downloading: return "Downloading original EPUB"
        case .downloaded:
            let progress = Int((book.progression * 100).rounded())
            return progress > 0 ? "Downloaded · \(progress)% read" : "Downloaded"
        case .failed: return "Download failed"
        case .missing: return "Download missing"
        }
    }
}

private struct DownloadRow: View {
    let item: OfflineItem
    let play: () -> Void
    let remove: () -> Void

    var body: some View {
        HStack(spacing: 14) {
            if let poster = item.posterFile,
               let image = UIImage(contentsOfFile: OfflineCatalog.localURL(for: poster).path) {
                Image(uiImage: image)
                    .resizable()
                    .scaledToFill()
                    .frame(width: 48, height: 72)
                    .clipShape(RoundedRectangle(cornerRadius: 7))
            } else {
                Image(systemName: item.isPlayable ? "play.fill" : stateIcon)
                    .frame(width: 48, height: 72)
                    .foregroundStyle(item.isPlayable ? Palette.accent : .secondary)
                    .background(Palette.surfaceHi, in: RoundedRectangle(cornerRadius: 7))
            }
            VStack(alignment: .leading, spacing: 5) {
                Text(item.title)
                    .font(.headline)
                    .foregroundStyle(Palette.onBg)
                if let context = item.context {
                    Text(context).font(.subheadline).foregroundStyle(.secondary)
                }
                HStack(spacing: 7) {
                    Text(stateLabel)
                    if let height = item.actualHeight { Text("\(height)p") }
                    if let bytes = item.bytesTotal { Text(Self.bytes(bytes)) }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                if item.state == .downloading, let total = item.bytesTotal, total > 0 {
                    ProgressView(value: Double(item.bytesDownloaded), total: Double(total))
                        .tint(Palette.accent)
                }
                if let error = item.errorMessage {
                    Text(error).font(.caption).foregroundStyle(.red)
                }
            }
            Spacer()
            Button(role: .destructive, action: remove) {
                Image(systemName: "trash")
            }
            .buttonStyle(.borderless)
        }
        .contentShape(Rectangle())
        .onTapGesture(perform: play)
    }

    private var stateIcon: String {
        switch item.state {
        case .queued, .preparing, .readyToTransfer: return "clock"
        case .downloading: return "arrow.down"
        case .paused: return "pause.fill"
        case .failed, .missing: return "exclamationmark.triangle"
        case .intent: return "ellipsis"
        case .downloaded: return "checkmark"
        }
    }

    private var stateLabel: String {
        switch item.state {
        case .intent, .queued: return "Queued"
        case .preparing: return "Preparing on server · keep or reopen Cinema to start transfer"
        case .readyToTransfer: return "Ready to download"
        case .downloading: return "Downloading"
        case .downloaded: return "Downloaded"
        case .paused: return "Paused — tap Resume"
        case .failed: return "Download failed"
        case .missing: return "Download missing"
        }
    }

    private static func bytes(_ value: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: value, countStyle: .file)
    }
}
#endif
