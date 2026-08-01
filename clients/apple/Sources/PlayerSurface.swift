import AVKit
import SwiftUI
import UIKit

enum PictureInPictureCommand: Equatable {
    case start
    case stop
    case unavailable
}

/// Owns the system Picture in Picture controller while PlayerView continues to
/// own the transport controls. Using the existing AVPlayerLayer avoids
/// reintroducing AVPlayerViewController's LIVE treatment for growing HLS
/// playlists.
@MainActor
final class PictureInPictureController: NSObject, ObservableObject,
                                        @preconcurrency AVPictureInPictureControllerDelegate {
    @Published private(set) var isSupported = AVPictureInPictureController.isPictureInPictureSupported()
    @Published private(set) var isPossible = false
    @Published private(set) var isActive = false
    @Published private(set) var errorMessage: String?

    private weak var playerLayer: AVPlayerLayer?
    private var controller: AVPictureInPictureController?
    private var possibleObservation: NSKeyValueObservation?
    private var activeObservation: NSKeyValueObservation?

    nonisolated static func command(isActive: Bool, isPossible: Bool) -> PictureInPictureCommand {
        if isActive { return .stop }
        return isPossible ? .start : .unavailable
    }

    func attach(to playerLayer: AVPlayerLayer) {
        guard self.playerLayer !== playerLayer || controller == nil else { return }
        detach()
        self.playerLayer = playerLayer
        isSupported = AVPictureInPictureController.isPictureInPictureSupported()
        guard isSupported else { return }

        let source = AVPictureInPictureController.ContentSource(playerLayer: playerLayer)
        let controller = AVPictureInPictureController(contentSource: source)
        controller.delegate = self
        #if os(iOS)
        controller.canStartPictureInPictureAutomaticallyFromInline = true
        #endif
        self.controller = controller

        possibleObservation = controller.observe(\.isPictureInPicturePossible,
                                                 options: [.initial, .new]) { [weak self] controller, _ in
            Task { @MainActor in
                self?.isPossible = controller.isPictureInPicturePossible
            }
        }
        activeObservation = controller.observe(\.isPictureInPictureActive,
                                               options: [.initial, .new]) { [weak self] controller, _ in
            Task { @MainActor in
                self?.isActive = controller.isPictureInPictureActive
            }
        }
    }

    func toggle() {
        errorMessage = nil
        switch Self.command(isActive: isActive, isPossible: isPossible) {
        case .start:
            controller?.startPictureInPicture()
        case .stop:
            controller?.stopPictureInPicture()
        case .unavailable:
            errorMessage = "Picture in Picture isn't ready yet."
        }
    }

    func stop() {
        if controller?.isPictureInPictureActive == true {
            controller?.stopPictureInPicture()
        }
    }

    func detach(resetPublishedState: Bool = true) {
        stop()
        possibleObservation = nil
        activeObservation = nil
        controller?.delegate = nil
        controller = nil
        playerLayer = nil
        if resetPublishedState {
            isPossible = false
            isActive = false
        }
    }

    func pictureInPictureControllerDidStartPictureInPicture(
        _ pictureInPictureController: AVPictureInPictureController
    ) {
        isActive = true
        errorMessage = nil
    }

    func pictureInPictureControllerDidStopPictureInPicture(
        _ pictureInPictureController: AVPictureInPictureController
    ) {
        isActive = false
    }

    func pictureInPictureController(
        _ pictureInPictureController: AVPictureInPictureController,
        failedToStartPictureInPictureWithError error: Error
    ) {
        isActive = false
        errorMessage = "Picture in Picture couldn't start: \(error.localizedDescription)"
    }

    func pictureInPictureController(
        _ pictureInPictureController: AVPictureInPictureController,
        restoreUserInterfaceForPictureInPictureStopWithCompletionHandler completionHandler: @escaping (Bool) -> Void
    ) {
        completionHandler(true)
    }
}

/// A video-only AVPlayer surface. Unlike SwiftUI's `VideoPlayer`, this view
/// does not install a second transport overlay or participate in the tvOS
/// focus engine; PlayerView remains the sole owner of playback controls.
struct PlayerSurface: UIViewRepresentable {
    let player: AVPlayer
    let pictureInPicture: PictureInPictureController

    func makeCoordinator() -> Coordinator {
        Coordinator(pictureInPicture: pictureInPicture)
    }

    func makeUIView(context: Context) -> PlayerSurfaceView {
        let view = PlayerSurfaceView()
        view.playerLayer.player = player
        context.coordinator.pictureInPicture.attach(to: view.playerLayer)
        return view
    }

    func updateUIView(_ view: PlayerSurfaceView, context: Context) {
        if view.playerLayer.player !== player {
            view.playerLayer.player = player
        }
        context.coordinator.pictureInPicture.attach(to: view.playerLayer)
    }

    static func dismantleUIView(_ view: PlayerSurfaceView, coordinator: Coordinator) {
        // SwiftUI is invalidating its observation graph while this callback
        // runs. Publishing from here violates Swift's exclusivity rules on
        // tvOS, so tear down the AVKit objects without notifying a view that
        // is already being destroyed. A surviving controller is reset by the
        // normal detach at the start of its next attachment.
        coordinator.pictureInPicture.detach(resetPublishedState: false)
        view.playerLayer.player = nil
    }

    final class Coordinator {
        let pictureInPicture: PictureInPictureController

        init(pictureInPicture: PictureInPictureController) {
            self.pictureInPicture = pictureInPicture
        }
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
