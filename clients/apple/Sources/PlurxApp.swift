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

/// Navigation targets shared by each top-level tab's navigation stack.
enum Route: Hashable {
    case collection(LibraryCollection)
    case item(Int)
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
                ConnectView(discovery: model.discovery)
            case .needLogin:
                LoginView()
            case .ready:
                HomeView()
            }
        }
    }
}
