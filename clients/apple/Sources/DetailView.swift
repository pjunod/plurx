import SwiftUI

/// Identifies a play request for the full-screen player cover.
struct PlayContext: Identifiable {
    let id = UUID()
    let itemId: Int
    let fileId: Int
    let startMs: Int
    let durationMs: Int
    let title: String
}

struct DetailView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    let itemId: Int
    @State private var detail: ItemDetail?
    @State private var play: PlayContext?
    @State private var loadError: String?
    @State private var watchBusy = false
    @State private var actionError: String?

    var body: some View {
        ScrollView {
            if let detail {
                content(detail)
            } else if let loadError {
                ContentUnavailableView(
                    "Couldn't load this title",
                    systemImage: "exclamationmark.triangle",
                    description: Text(loadError)
                )
                .frame(maxWidth: .infinity).padding(.top, 80)
            } else {
                ProgressView().tint(Palette.accent)
                    .frame(maxWidth: .infinity).padding(.top, 80)
            }
        }
        .background(Palette.bg.ignoresSafeArea())
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .task(id: itemId) {
            do {
                detail = try await model.itemDetail(itemId)
                loadError = nil
            } catch {
                loadError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            }
        }
        .fullScreenCover(item: $play) { ctx in
            PlayerView(itemId: ctx.itemId, fileId: ctx.fileId, startMs: ctx.startMs,
                       durationMs: ctx.durationMs, title: ctx.title,
                       onPlayNext: { play = $0 })
                .id(ctx.id)
                .environmentObject(model)
        }
    }

    @ViewBuilder
    private func content(_ detail: ItemDetail) -> some View {
        let item = detail.item
        let file = detail.files?.first
        let durationMs = file?.durationMs ?? item.runtimeMs
        let resumeMs = item.watch?.positionMs ?? 0
        let nearlyDone = (durationMs ?? 0) > 0 && Double(resumeMs) > Double(durationMs!) * 0.95
        let canResume = resumeMs > 3000 && !nearlyDone

        VStack(alignment: .leading, spacing: 0) {
            ZStack(alignment: .bottom) {
                AuthImage(path: item.backdrop ?? item.poster)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .clipped()
                LinearGradient(
                    colors: [.clear, Palette.bg],
                    startPoint: .top, endPoint: .bottom
                )
            }
            .frame(maxWidth: .infinity)
            .frame(height: heroHeight)
            .clipped()

            VStack(alignment: .leading, spacing: 12) {
                Text(item.title)
                    #if os(tvOS)
                    .font(.system(size: 54, weight: .bold))
                    #else
                    .font(.largeTitle.bold())
                    #endif
                    .foregroundColor(Palette.onBg)
                    .fixedSize(horizontal: false, vertical: true)
                Text(metaLine(item, durationMs: durationMs))
                    .font(.system(.callout, design: .monospaced))
                    .foregroundColor(Palette.muted)
                    .fixedSize(horizontal: false, vertical: true)

                if let file, item.isPlayable {
                    playbackActions(
                        item: item,
                        file: file,
                        durationMs: durationMs ?? 0,
                        resumeMs: resumeMs,
                        canResume: canResume
                    )
                    .padding(.top, 4)
                }

                watchButton(detail)

                if let actionError {
                    Text(actionError)
                        .font(.caption)
                        .foregroundColor(Palette.accent)
                }

                if let overview = item.overview, !overview.isEmpty {
                    Text(overview)
                        .font(.body)
                        .foregroundColor(Palette.onBg.opacity(0.78))
                        .lineSpacing(4)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, 8)
                }
            }
            .frame(maxWidth: 980, alignment: .leading)
            .padding(.horizontal, screenHPad)
            .padding(.top, 8)

            if let children = detail.children, !children.isEmpty {
                MediaRow(title: childrenHeading(item.kind), items: children)
                    .padding(.top, 14)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, 30)
    }

    private func watchButton(_ detail: ItemDetail) -> some View {
        let watched = isWatched(detail.item)
        return Button {
            Task { await toggleWatched(detail.item, watched: watched) }
        } label: {
            Label(
                watched ? "Mark unwatched" : "Mark watched",
                systemImage: watched ? "checkmark.circle.fill" : "checkmark.circle"
            )
            .font(.system(.body, design: .monospaced))
        }
        .buttonStyle(.bordered)
        .tint(watched ? Palette.accent : Palette.muted)
        .disabled(watchBusy)
        #if os(tvOS)
        .fixedSize()
        #endif
    }

    private func isWatched(_ item: Item) -> Bool {
        if let rollup = item.rollup, rollup.leaves > 0 {
            return rollup.watched >= rollup.leaves
        }
        return item.watch?.watched == true
    }

    private func toggleWatched(_ item: Item, watched: Bool) async {
        watchBusy = true
        actionError = nil
        do {
            try await model.setWatched(itemId: item.id, watched: !watched)
            detail = try await model.itemDetail(item.id)
            await model.loadHome()
        } catch {
            actionError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        }
        watchBusy = false
    }

    private var heroHeight: CGFloat {
        #if os(tvOS)
        return 520
        #else
        return horizontalSizeClass == .regular ? 430 : 270
        #endif
    }

    @ViewBuilder
    private func playbackActions(
        item: Item,
        file: MediaFile,
        durationMs: Int,
        resumeMs: Int,
        canResume: Bool
    ) -> some View {
        #if os(tvOS)
        HStack(spacing: 14) {
            resumeButton(item: item, file: file, durationMs: durationMs,
                         resumeMs: resumeMs, canResume: canResume)
                .fixedSize()
            if canResume {
                startOverButton(item: item, file: file, durationMs: durationMs)
            }
        }
        #else
        let actionLayout = horizontalSizeClass == .compact
            ? AnyLayout(VStackLayout(spacing: 10))
            : AnyLayout(HStackLayout(spacing: 14))

        actionLayout {
            resumeButton(item: item, file: file, durationMs: durationMs,
                         resumeMs: resumeMs, canResume: canResume)
            if canResume {
                startOverButton(item: item, file: file, durationMs: durationMs)
            }
        }
        .frame(maxWidth: .infinity)
        #endif
    }

    private func resumeButton(
        item: Item,
        file: MediaFile,
        durationMs: Int,
        resumeMs: Int,
        canResume: Bool
    ) -> some View {
        PrimaryButton(title: canResume ? "▶  Resume · \(formatTime(resumeMs))" : "▶  Play") {
            play = PlayContext(
                itemId: item.id,
                fileId: file.id,
                startMs: canResume ? resumeMs : 0,
                durationMs: durationMs,
                title: item.title
            )
        }
    }

    private func startOverButton(item: Item, file: MediaFile, durationMs: Int) -> some View {
        Button {
            play = PlayContext(
                itemId: item.id,
                fileId: file.id,
                startMs: 0,
                durationMs: durationMs,
                title: item.title
            )
        } label: {
            Text("Start over")
                .font(.system(.body, design: .monospaced))
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.bordered)
        .tint(Palette.muted)
        #if os(iOS)
        .controlSize(.large)
        #endif
    }

    private func metaLine(_ item: Item, durationMs: Int?) -> String {
        var parts: [String] = []
        if item.kind == "episode" {
            if let show = item.showTitle { parts.append(show) }
            if let s = item.seasonNumber, let e = item.episodeNumber { parts.append("S\(s) · E\(e)") }
        }
        if let y = item.year { parts.append(String(y)) }
        if let d = durationMs, d > 0 { parts.append(formatTime(d)) }
        return parts.joined(separator: "   ·   ")
    }

    private func childrenHeading(_ kind: String) -> String {
        switch kind {
        case "show": return "Seasons"
        case "season": return "Episodes"
        default: return "Contents"
        }
    }
}
