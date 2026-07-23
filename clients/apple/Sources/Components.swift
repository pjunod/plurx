import SwiftUI

#if os(tvOS)
let screenHPad: CGFloat = 50
#else
let screenHPad: CGFloat = 20
#endif

/// A poster tile with a thin resume bar. Focus affordance (tvOS lift / touch
/// highlight) comes from wrapping it in a `NavigationLink.posterButtonStyle()`.
struct PosterCard: View {
    let item: Item
    var width: CGFloat = 120

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ZStack(alignment: .bottomLeading) {
                AuthImage(path: item.poster)
                    .frame(width: width, height: width * 3 / 2)
                    .clipped()
                    .cornerRadius(8)

                let frac = progressFraction(item.watch, runtimeMs: item.runtimeMs)
                if frac > 0 {
                    Rectangle()
                        .fill(Palette.accent)
                        .frame(width: width * CGFloat(frac), height: 3)
                }
            }
            Text(item.title)
                .font(.system(.caption, design: .monospaced)).fontWeight(.semibold)
                .foregroundColor(Palette.onBg).lineLimit(1)
            Text(mediaSubtitle(item))
                .font(.system(.caption2, design: .monospaced))
                .foregroundColor(Palette.muted).lineLimit(1)
        }
        .frame(width: width, alignment: .leading)
    }
}

/// A titled horizontal shelf of posters. Renders nothing when empty.
struct MediaRow: View {
    let title: String
    let items: [Item]

    var body: some View {
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 10) {
                Text(title)
                    .font(.system(.title3, design: .monospaced)).fontWeight(.semibold)
                    .foregroundColor(Palette.onBg)
                    .padding(.horizontal, screenHPad)
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 14) {
                        ForEach(items) { item in
                            NavigationLink(value: Route.item(item.id)) {
                                PosterCard(item: item)
                            }
                            .posterButtonStyle()
                        }
                    }
                    .padding(.horizontal, screenHPad)
                }
            }
            .padding(.vertical, 10)
        }
    }
}
