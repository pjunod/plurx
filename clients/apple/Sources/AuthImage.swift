import SwiftUI
import UIKit

/// Async poster/backdrop image. Loads `/api/v1/images/…` with the bearer header
/// (URLSession's cache handles repeats), showing a muted placeholder until then.
struct AuthImage: View {
    let path: String?
    @State private var image: UIImage?

    var body: some View {
        ZStack {
            if let image {
                Image(uiImage: image)
                    .resizable()
                    .scaledToFill()
            } else {
                Palette.surfaceHi
            }
        }
        .task(id: path) { await load() }
    }

    private func load() async {
        image = nil
        guard let path, let url = Session.shared.url(path) else { return }
        var req = URLRequest(url: url)
        Session.shared.authorize(&req)
        if let (data, _) = try? await URLSession.shared.data(for: req),
           let ui = UIImage(data: data) {
            image = ui
        }
    }
}
