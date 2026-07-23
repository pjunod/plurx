import AVKit
import SwiftUI

/// Full-screen player. `VideoPlayer` gives the native transport, scrubber, and
/// audio/subtitle track menus on both platforms (and the native info panel on
/// tvOS); this view adds resume/progress wiring via [PlayerController] and an
/// exit affordance — a close button on iOS, the Menu button on tvOS.
struct PlayerView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    let itemId: Int
    let fileId: Int
    let startMs: Int
    let durationMs: Int
    let title: String

    @StateObject private var controller = PlayerController()

    var body: some View {
        ZStack(alignment: .topLeading) {
            Color.black.ignoresSafeArea()

            VideoPlayer(player: controller.player)
                .ignoresSafeArea()

            if controller.failed {
                VStack(spacing: 14) {
                    Text("Couldn't start playback.")
                        .font(.system(.body, design: .monospaced))
                        .foregroundColor(.white)
                    Button("Close") { dismiss() }
                        .buttonStyle(.borderedProminent)
                        .tint(Palette.accent)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black)
            }

            #if os(iOS)
            Button { dismiss() } label: {
                Image(systemName: "xmark.circle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.white.opacity(0.9))
                    .padding(20)
            }
            .buttonStyle(.plain)
            #endif
        }
        .task {
            controller.start(model: model, itemId: itemId, fileId: fileId,
                             startMs: startMs, durationMs: durationMs, title: title)
        }
        .onDisappear { controller.stop() }
        #if os(tvOS)
        .onExitCommand { dismiss() }
        #endif
    }
}
