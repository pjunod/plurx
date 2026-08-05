import SwiftUI

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
    #if os(iOS)
    @ObservedObject private var downloads = OfflineDownloadManager.shared
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
                HomeView()
            }
        }
    }
}
