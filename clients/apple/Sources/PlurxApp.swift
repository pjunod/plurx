import SwiftUI

#if DEBUG
/// Command-line-only entry point for physical-device acceptance. `devicectl`
/// can supply these defaults when it launches a signed debug build, so the
/// test rig does not need a person to navigate the app or time remote presses.
struct PlaybackAcceptanceLaunch: Equatable {
    let itemId: Int
    let fileId: Int
    let startMs: Int
    let durationMs: Int
    let title: String
    let height: Int?
    let probesEnabled: Bool

    static func current(defaults: UserDefaults = .standard) -> Self? {
        let fileId = defaults.integer(forKey: "plurx.acceptance.fileId")
        guard fileId > 0 else { return nil }
        let height = defaults.integer(forKey: "plurx.acceptance.height")
        return Self(
            itemId: max(0, defaults.integer(forKey: "plurx.acceptance.itemId")),
            fileId: fileId,
            startMs: max(0, defaults.integer(forKey: "plurx.acceptance.startMs")),
            durationMs: max(0, defaults.integer(forKey: "plurx.acceptance.durationMs")),
            title: defaults.string(forKey: "plurx.acceptance.title") ?? "Playback acceptance",
            height: height > 0 ? height : nil,
            probesEnabled: defaults.bool(forKey: "plurx.acceptance.probe")
        )
    }
}
#endif

@main
struct PlurxApp: App {
    #if os(iOS)
    @UIApplicationDelegateAdaptor(OfflineAppDelegate.self) private var appDelegate
    #endif
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(model)
                .preferredColorScheme(model.appearance.preferredColorScheme)
                .fontDesign(model.theme.fontDesign)
                .tint(Palette.accent)
        }
    }
}

/// Navigation targets shared by each top-level tab's navigation stack.
enum Route: Hashable {
    case collection(LibraryCollection)
    case item(Int)
}

struct RootView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.scenePhase) private var scenePhase
    #if os(iOS)
    @ObservedObject private var downloads = OfflineDownloadManager.shared
    #endif
    #if DEBUG
    private let playbackAcceptance = PlaybackAcceptanceLaunch.current()
    #endif

    var body: some View {
        ZStack {
            Palette.bg.ignoresSafeArea()
            switch model.phase {
            case .loading:
                #if os(iOS)
                if downloads.items.isEmpty {
                    ProgressView().tint(Palette.accent)
                } else {
                    NavigationStack { DownloadsView() }
                }
                #else
                ProgressView().tint(Palette.accent)
                #endif
            case .needServer:
                ConnectView(discovery: model.discovery)
            case .needLogin:
                LoginView()
            case .reconnectFailed:
                #if os(iOS)
                if downloads.items.isEmpty {
                    ReconnectView()
                } else {
                    NavigationStack { DownloadsView() }
                }
                #else
                ReconnectView()
                #endif
            case .ready:
                #if DEBUG
                if let launch = playbackAcceptance {
                    PlayerView(
                        itemId: launch.itemId,
                        fileId: launch.fileId,
                        startMs: launch.startMs,
                        durationMs: launch.durationMs,
                        title: launch.title,
                        initialHeight: launch.height,
                        diagnosticProbesEnabled: launch.probesEnabled
                    )
                } else {
                    HomeView()
                }
                #else
                HomeView()
                #endif
            }
        }
        #if os(iOS)
        .onChange(of: scenePhase) { _, phase in
            guard phase == .active else { return }
            Task { await downloads.resumePendingPreparation() }
        }
        .onChange(of: model.phase) { _, phase in
            guard phase == .ready else { return }
            Task { await downloads.resumePendingPreparation() }
        }
        #endif
    }
}
