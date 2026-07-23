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

#if os(tvOS)
private let backdropHeight: CGFloat = 380
#else
private let backdropHeight: CGFloat = 240
#endif

struct DetailView: View {
    @EnvironmentObject var model: AppModel
    let itemId: Int
    @State private var detail: ItemDetail?
    @State private var play: PlayContext?

    var body: some View {
        ScrollView {
            if let detail {
                content(detail)
            } else {
                ProgressView().tint(Palette.accent)
                    .frame(maxWidth: .infinity).padding(.top, 80)
            }
        }
        .background(Palette.bg.ignoresSafeArea())
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .task(id: itemId) { detail = try? await model.itemDetail(itemId) }
        .fullScreenCover(item: $play) { ctx in
            PlayerView(itemId: ctx.itemId, fileId: ctx.fileId, startMs: ctx.startMs, title: ctx.title)
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
                    .frame(height: backdropHeight)
                    .clipped()
                LinearGradient(
                    colors: [.clear, Palette.bg],
                    startPoint: .top, endPoint: .bottom
                )
                .frame(height: backdropHeight)
            }

            VStack(alignment: .leading, spacing: 12) {
                Text(item.title)
                    .font(.system(.largeTitle, design: .monospaced)).fontWeight(.bold)
                    .foregroundColor(Palette.onBg)
                Text(metaLine(item, durationMs: durationMs))
                    .font(.system(.callout, design: .monospaced))
                    .foregroundColor(Palette.muted)

                if let file, item.isMovieOrEpisode {
                    HStack(spacing: 14) {
                        PrimaryButton(title: canResume ? "▶  Resume · \(formatTime(resumeMs))" : "▶  Play") {
                            play = PlayContext(itemId: item.id, fileId: file.id,
                                               startMs: canResume ? resumeMs : 0,
                                               durationMs: durationMs ?? 0, title: item.title)
                        }
                        .fixedSize()
                        if canResume {
                            Button("Start over") {
                                play = PlayContext(itemId: item.id, fileId: file.id, startMs: 0,
                                                   durationMs: durationMs ?? 0, title: item.title)
                            }
                            .font(.system(.body, design: .monospaced))
                            .buttonStyle(.bordered)
                            .tint(Palette.muted)
                        }
                    }
                    .padding(.top, 4)
                }

                if let overview = item.overview, !overview.isEmpty {
                    Text(overview)
                        .font(.system(.body, design: .monospaced))
                        .foregroundColor(Palette.muted)
                        .padding(.top, 8)
                }
            }
            .padding(.horizontal, screenHPad)
            .padding(.top, 8)

            if let children = detail.children, !children.isEmpty {
                MediaRow(title: childrenHeading(item.kind), items: children)
                    .padding(.top, 14)
            }
        }
        .padding(.bottom, 30)
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
