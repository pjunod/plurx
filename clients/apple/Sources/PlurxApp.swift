import SwiftUI

@main
struct PlurxApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(model)
                .preferredColorScheme(.dark)
                .tint(Palette.accent)
        }
    }
}

/// Navigation targets pushed onto the home stack.
enum Route: Hashable {
    case library(Library)
    case item(Int)
    case settings
}

struct RootView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        ZStack {
            Palette.bg.ignoresSafeArea()
            switch model.phase {
            case .loading:
                ProgressView().tint(Palette.accent)
            case .needServer:
                ConnectView()
            case .needLogin:
                LoginView()
            case .ready:
                HomeView()
            }
        }
    }
}
