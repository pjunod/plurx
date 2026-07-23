import SwiftUI

#if os(tvOS)
private let cardWidth: CGFloat = 180
#else
private let cardWidth: CGFloat = 116
#endif

struct LibraryView: View {
    @EnvironmentObject var model: AppModel
    let library: Library
    @State private var items: [Item]?

    private var columns: [GridItem] {
        [GridItem(.adaptive(minimum: cardWidth), spacing: 16, alignment: .top)]
    }

    var body: some View {
        ScrollView {
            if let items {
                if items.isEmpty {
                    Text("This library is empty.")
                        .foregroundColor(Palette.muted)
                        .frame(maxWidth: .infinity).padding(.top, 60)
                } else {
                    LazyVGrid(columns: columns, alignment: .leading, spacing: 20) {
                        ForEach(items) { item in
                            NavigationLink(value: Route.item(item.id)) {
                                PosterCard(item: item, width: cardWidth)
                            }
                            .posterButtonStyle()
                        }
                    }
                    .padding(screenHPad)
                }
            } else {
                ProgressView().tint(Palette.accent)
                    .frame(maxWidth: .infinity).padding(.top, 60)
            }
        }
        .background(Palette.bg.ignoresSafeArea())
        .navigationTitle(library.name)
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .task(id: library.id) {
            items = (try? await model.libraryItems(library.id)) ?? []
        }
    }
}
