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
    #if os(iOS)
    @Published private(set) var isSupported = AVPictureInPictureController.isPictureInPictureSupported()
    #else
    @Published private(set) var isSupported = false
    #endif
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
        #if os(iOS)
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
        #endif
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
    let pgsOverlay: PGSOverlayWindow?

    func makeCoordinator() -> Coordinator {
        Coordinator(pictureInPicture: pictureInPicture)
    }

    func makeUIView(context: Context) -> PlayerSurfaceView {
        let view = PlayerSurfaceView()
        view.playerLayer.player = player
        view.applyPGSOverlay(pgsOverlay, to: player.currentItem)
        context.coordinator.pictureInPicture.attach(to: view.playerLayer)
        return view
    }

    func updateUIView(_ view: PlayerSurfaceView, context: Context) {
        if view.playerLayer.player !== player {
            view.playerLayer.player = player
        }
        view.applyPGSOverlay(pgsOverlay, to: player.currentItem)
        context.coordinator.pictureInPicture.attach(to: view.playerLayer)
    }

    static func dismantleUIView(_ view: PlayerSurfaceView, coordinator: Coordinator) {
        // SwiftUI is invalidating its observation graph while this callback
        // runs. Publishing from here violates Swift's exclusivity rules on
        // tvOS, so tear down the AVKit objects without notifying a view that
        // is already being destroyed. A surviving controller is reset by the
        // normal detach at the start of its next attachment.
        coordinator.pictureInPicture.detach(resetPublishedState: false)
        view.applyPGSOverlay(nil, to: nil)
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
    let playerLayer = AVPlayerLayer()

    private struct OverlayNode {
        let layer: CALayer
        let object: PGSOverlayObject
        let canvasWidth: Int
        let canvasHeight: Int
    }

    private var synchronizedLayer: AVSynchronizedLayer?
    private var overlayNodes: [OverlayNode] = []
    private var overlayRevision: Int?
    private weak var overlayItem: AVPlayerItem?
    private var videoRectObservation: NSKeyValueObservation?

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        isUserInteractionEnabled = false
        isAccessibilityElement = false
        playerLayer.videoGravity = .resizeAspect
        layer.addSublayer(playerLayer)
        clipsToBounds = true
        videoRectObservation = playerLayer.observe(\.videoRect, options: [.new]) {
            [weak self] _, _ in
            Task { @MainActor in self?.setNeedsLayout() }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is unavailable")
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        playerLayer.frame = bounds
        let videoRect = playerLayer.videoRect
        synchronizedLayer?.frame = videoRect
        let destination = CGRect(origin: .zero, size: videoRect.size)
        for node in overlayNodes {
            node.layer.frame = PGSOverlayPolicy.objectFrame(
                node.object,
                canvasWidth: node.canvasWidth,
                canvasHeight: node.canvasHeight,
                destination: destination
            )
        }
        CATransaction.commit()
    }

    func applyPGSOverlay(_ window: PGSOverlayWindow?, to item: AVPlayerItem?) {
        guard let window, let item else {
            detachPGSOverlay()
            return
        }
        guard overlayRevision != window.revision || overlayItem !== item else {
            setNeedsLayout()
            return
        }

        detachPGSOverlay()
        overlayRevision = window.revision
        overlayItem = item

        let synchronized = AVSynchronizedLayer(playerItem: item)
        synchronized.masksToBounds = true
        synchronizedLayer = synchronized
        layer.addSublayer(synchronized)

        for renderable in window.cues {
            let cue = renderable.cue
            let unclampedStart = PGSOverlayPolicy.itemTimeMs(
                sourceTimeMs: cue.startMs,
                baseMs: window.baseMs
            )
            let itemEnd = PGSOverlayPolicy.itemTimeMs(
                sourceTimeMs: cue.endMs,
                baseMs: window.baseMs
            )
            let itemStart = max(0, unclampedStart)
            guard itemEnd > itemStart else { continue }

            for object in renderable.objects {
                let imageLayer = CALayer()
                imageLayer.contents = object.image
                imageLayer.contentsGravity = .resize
                imageLayer.opacity = 0
                imageLayer.actions = [
                    "bounds": NSNull(),
                    "position": NSNull(),
                    "opacity": NSNull(),
                ]

                let visibility = CABasicAnimation(keyPath: "opacity")
                visibility.fromValue = 1
                visibility.toValue = 1
                visibility.beginTime = AVCoreAnimationBeginTimeAtZero
                    + Double(itemStart) / 1_000
                visibility.duration = Double(itemEnd - itemStart) / 1_000
                visibility.fillMode = .removed
                visibility.isRemovedOnCompletion = true
                imageLayer.add(visibility, forKey: "pgs-visibility")
                synchronized.addSublayer(imageLayer)
                overlayNodes.append(OverlayNode(
                    layer: imageLayer,
                    object: object.object,
                    canvasWidth: cue.canvasWidth,
                    canvasHeight: cue.canvasHeight
                ))
            }
        }
        setNeedsLayout()
    }

    private func detachPGSOverlay() {
        synchronizedLayer?.removeFromSuperlayer()
        synchronizedLayer = nil
        overlayNodes.removeAll(keepingCapacity: false)
        overlayRevision = nil
        overlayItem = nil
    }

    /// Test seam for the item-replacement invariant. The renderer must never
    /// leave the predecessor item's synchronized layer attached.
    func hasPGSOverlay(revision: Int, item: AVPlayerItem) -> Bool {
        overlayRevision == revision && overlayItem === item && synchronizedLayer != nil
    }

    var pgsOverlayNodeCount: Int { overlayNodes.count }

    deinit { videoRectObservation = nil }

    #if os(tvOS)
    override var canBecomeFocused: Bool { false }
    #endif
}
