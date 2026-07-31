import AVFoundation
import SwiftUI
import UIKit

/// A video-only AVPlayer surface. Unlike SwiftUI's `VideoPlayer`, this view
/// does not install a second transport overlay or participate in the tvOS
/// focus engine; PlayerView remains the sole owner of playback controls.
struct PlayerSurface: UIViewRepresentable {
    let player: AVPlayer

    func makeUIView(context: Context) -> PlayerSurfaceView {
        let view = PlayerSurfaceView()
        view.playerLayer.player = player
        return view
    }

    func updateUIView(_ view: PlayerSurfaceView, context: Context) {
        if view.playerLayer.player !== player {
            view.playerLayer.player = player
        }
    }

    static func dismantleUIView(_ view: PlayerSurfaceView, coordinator: Void) {
        view.playerLayer.player = nil
    }
}

final class PlayerSurfaceView: UIView {
    override class var layerClass: AnyClass { AVPlayerLayer.self }

    var playerLayer: AVPlayerLayer { layer as! AVPlayerLayer }

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        isUserInteractionEnabled = false
        isAccessibilityElement = false
        playerLayer.videoGravity = .resizeAspect
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is unavailable")
    }

    #if os(tvOS)
    override var canBecomeFocused: Bool { false }
    #endif
}
