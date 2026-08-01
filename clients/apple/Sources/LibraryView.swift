import SwiftUI

struct LibraryView: View {
    @EnvironmentObject var model: AppModel
    let collection: LibraryCollection

    @State private var items: [Item] = []
    @State private var sort: LibrarySort = .title
    @State private var filter: WatchFilter = .all
    @State private var loading = true
    @State private var error: String?

    private var visibleItems: [Item] {
        items.filter { AppModel.matches($0, filter: filter) }
    }

    private var sorts: [LibrarySort] {
        LibrarySort.allCases.filter { $0 != .recorded || collection.supportsRecordedSort }
    }

    private var columns: [GridItem] {
        [GridItem(.adaptive(minimum: model.posterSize.posterWidth), spacing: 18, alignment: .top)]
    }

    private var loadKey: String { "\(collection.id):\(sort.rawValue)" }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                summary
                stateContent
            }
            .padding(.horizontal, screenHPad)
            .padding(.bottom, 36)
        }
        .background(Palette.bg.ignoresSafeArea())
        .navigationTitle(collection.title)
        #if os(iOS)
        .navigationBarTitleDisplayMode(.large)
        .refreshable { await load() }
        #endif
        .toolbar { libraryToolbar }
        .task(id: loadKey) { await load() }
    }

    private var summary: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 4) {
                Text("\(visibleItems.count) \(visibleItems.count == 1 ? "item" : "items")")
                    .font(.system(.subheadline, design: .monospaced))
                    .foregroundColor(Palette.muted)
                if collection.libraries.count > 1 {
                    Text(collection.libraries.map(\.name).joined(separator: "  ·  "))
                        .font(.caption)
                        .foregroundColor(Palette.muted)
                        .lineLimit(2)
                }
            }
            Spacer()
        }
        .padding(.top, 8)
    }

    @ViewBuilder
    private var stateContent: some View {
        if loading && items.isEmpty {
            ProgressView().tint(Palette.accent)
                .frame(maxWidth: .infinity).padding(.top, 80)
        } else if let error, items.isEmpty {
            ContentUnavailableView(
                "Couldn't load \(collection.title)",
                systemImage: "exclamationmark.triangle",
                description: Text(error)
            )
            .frame(maxWidth: .infinity)
        } else if visibleItems.isEmpty {
            ContentUnavailableView(
                filter == .all ? "This library is empty" : "No matching titles",
                systemImage: "rectangle.stack",
                description: filter == .all ? nil : Text("Try a different watch filter.")
            )
            .frame(maxWidth: .infinity)
        } else {
            LazyVGrid(columns: columns, alignment: .leading, spacing: 24) {
                ForEach(visibleItems) { item in
                    NavigationLink(value: Route.item(item.id)) {
                        PosterCard(
                            item: item,
                            width: model.posterSize.posterWidth
                        )
                    }
                    .posterButtonStyle()
                }
            }
        }
    }

    @ToolbarContentBuilder
    private var libraryToolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .automatic) {
            Menu {
                Picker("Sort", selection: $sort) {
                    ForEach(sorts) { option in
                        Label(option.label, systemImage: option.icon).tag(option)
                    }
                }
            } label: {
                Label("Sort", systemImage: "arrow.up.arrow.down")
            }

            Menu {
                Picker("Watch status", selection: $filter) {
                    ForEach(WatchFilter.allCases) { option in
                        Text(option.label).tag(option)
                    }
                }
            } label: {
                Label("Filter", systemImage: filter == .all ? "line.3.horizontal.decrease" : "line.3.horizontal.decrease.circle.fill")
            }
        }
    }

    private func load() async {
        loading = true
        error = nil
        do {
            items = try await model.libraryItems(collection, sort: sort)
        } catch {
            self.error = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        }
        loading = false
    }
}
