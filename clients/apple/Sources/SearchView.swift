import SwiftUI

struct SearchView: View {
    @EnvironmentObject var model: AppModel
    @State private var query = ""
    @State private var results: [Item] = []
    @State private var searching = false
    @State private var hasSearched = false
    @State private var failure: ConnectionFailure?
    /// Bumped by the error state's Retry so `.task(id:)` re-runs the same
    /// query instead of requiring the viewer to retype it.
    @State private var reloadNonce = 0

    private var columns: [GridItem] {
        [GridItem(.adaptive(minimum: model.posterSize.posterWidth), spacing: 18, alignment: .top)]
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                #if os(tvOS)
                TextField("Search movies, shows, and episodes", text: $query)
                    .padding(.horizontal, screenHPad)
                    .padding(.top, 8)
                #endif

                searchContent
                    .padding(.horizontal, screenHPad)
            }
            .padding(.bottom, 36)
        }
        .background(Palette.bg.ignoresSafeArea())
        .navigationTitle("Search")
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        .searchable(text: $query, prompt: "Movies, shows, and episodes")
        #endif
        .task(id: "\(query)|\(reloadNonce)") { await performSearch() }
    }

    @ViewBuilder
    private var searchContent: some View {
        if searching {
            ProgressView().tint(Palette.accent)
                .frame(maxWidth: .infinity).padding(.top, 70)
        } else if let failure {
            ConnectionErrorView(
                failure: failure,
                server: model.serverLabel,
                retry: { reloadNonce += 1 },
                changeServer: { model.changeServer() }
            )
        } else if query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            ContentUnavailableView(
                "Find something to watch",
                systemImage: "magnifyingglass",
                description: Text("Search across every library share on this Cinema server.")
            )
            .frame(maxWidth: .infinity)
        } else if hasSearched && results.isEmpty {
            ContentUnavailableView(
                "No results",
                systemImage: "magnifyingglass",
                description: Text("Try another title or a broader phrase.")
            )
            .frame(maxWidth: .infinity)
        } else {
            Text("\(results.count) \(results.count == 1 ? "result" : "results")")
                .font(.system(.subheadline, design: .monospaced))
                .foregroundColor(Palette.muted)
            LazyVGrid(columns: columns, alignment: .leading, spacing: 24) {
                ForEach(results) { item in
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

    private func performSearch() async {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            results = []
            hasSearched = false
            failure = nil
            searching = false
            return
        }

        // Cleared before the debounce, not after it: leaving the previous
        // failure painted for 300 ms is long enough for Try again to look
        // dead. `searching` deliberately stays behind the sleep — raising the
        // spinner on every keystroke is what the debounce exists to avoid.
        failure = nil
        do {
            try await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            searching = true
            results = try await model.search(trimmed)
            hasSearched = true
        } catch is CancellationError {
            return
        } catch {
            // A search has no cached content to protect: the previous query's
            // results are not an answer to this one, so this surface is always
            // the full state.
            failure = AppModel.connectionFailure(for: error)
            hasSearched = true
        }
        searching = false
    }
}
