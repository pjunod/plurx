import SwiftUI

struct HomeView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    header
                    if model.homeLoading {
                        ProgressView().tint(Palette.accent)
                            .frame(maxWidth: .infinity).padding(.top, 60)
                    } else if let err = model.homeError {
                        Text(err).foregroundColor(Palette.muted)
                            .frame(maxWidth: .infinity).padding(.top, 60)
                    } else {
                        content
                    }
                }
                .padding(.bottom, 24)
            }
            .background(Palette.bg.ignoresSafeArea())
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .library(let lib): LibraryView(library: lib)
                case .item(let id): DetailView(itemId: id)
                case .settings: SettingsView()
                }
            }
            .task { if model.homeLoading { await model.loadHome() } }
        }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("plurx")
                .font(.system(size: 30, weight: .bold, design: .monospaced))
                .foregroundColor(Palette.accent)
            Spacer()
            if let name = model.username {
                Text(name)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(Palette.muted)
            }
            NavigationLink(value: Route.settings) {
                Image(systemName: "gearshape")
                    .font(.title3)
                    .foregroundColor(Palette.muted)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, screenHPad)
        .padding(.top, 14)
        .padding(.bottom, 6)
    }

    @ViewBuilder
    private var content: some View {
        if !model.libraries.isEmpty {
            Text("Libraries")
                .font(.system(.title3, design: .monospaced)).fontWeight(.semibold)
                .foregroundColor(Palette.onBg)
                .padding(.horizontal, screenHPad).padding(.top, 8)
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 12) {
                    ForEach(model.libraries) { lib in
                        NavigationLink(value: Route.library(lib)) {
                            LibraryChip(library: lib)
                        }
                        .posterButtonStyle()
                    }
                }
                .padding(.horizontal, screenHPad).padding(.vertical, 8)
            }
        }

        MediaRow(title: "Continue Watching", items: model.hubs.continueWatching ?? [])
        MediaRow(title: "Next Up", items: model.hubs.nextUp ?? [])
        MediaRow(title: "Recently Added", items: model.hubs.recentlyAdded ?? [])

        let empty = (model.hubs.continueWatching ?? []).isEmpty
            && (model.hubs.nextUp ?? []).isEmpty
            && (model.hubs.recentlyAdded ?? []).isEmpty
        if empty && model.libraries.isEmpty {
            Text("Nothing here yet — add a library on your server.")
                .foregroundColor(Palette.muted)
                .frame(maxWidth: .infinity).padding(.top, 60)
        }
    }
}

struct LibraryChip: View {
    let library: Library

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(library.name)
                .font(.system(.headline, design: .monospaced))
                .foregroundColor(Palette.onBg)
            Text(library.kind.prefix(1).uppercased() + library.kind.dropFirst())
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(Palette.muted)
        }
        .padding(.horizontal, 22).padding(.vertical, 16)
        .background(Palette.surfaceHi)
        .cornerRadius(10)
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Palette.outline, lineWidth: 1))
    }
}
