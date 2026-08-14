import AVKit
import SwiftUI
import UIKit

enum PictureInPictureCommand: Equatable {
    case start
    case stop
    case unavailable
}

struct PictureInPictureControlState: Equatable {
    let isButtonEnabled: Bool
    let command: PictureInPictureCommand
    let messageOnTap: String?
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
    private var unavailableMessageTask: Task<Void, Never>?

    nonisolated static func command(isActive: Bool, isPossible: Bool) -> PictureInPictureCommand {
        if isActive { return .stop }
        return isPossible ? .start : .unavailable
    }

    /// The rendered control must remain reachable whenever this device supports
    /// PiP. Availability is an outcome of a tap, not permission to disable the
    /// only path that can explain why AVKit cannot start yet. Requiring the
    /// backing controller here also turns a stale published `isPossible` value
    /// after surface teardown into a visible unavailable result instead of a
    /// start message sent through a nil optional.
    nonisolated static func controlState(
        isSupported: Bool,
        isActive: Bool,
        isPossible: Bool,
        hasAttachedController: Bool
    ) -> PictureInPictureControlState {
        guard isSupported else {
            return PictureInPictureControlState(
                isButtonEnabled: false,
                command: .unavailable,
                messageOnTap: nil
            )
        }
        guard hasAttachedController else {
            return PictureInPictureControlState(
                isButtonEnabled: true,
                command: .unavailable,
                messageOnTap: "Picture in Picture isn't ready yet."
            )
        }
        let command = command(isActive: isActive, isPossible: isPossible)
        return PictureInPictureControlState(
            isButtonEnabled: true,
            command: command,
            messageOnTap: command == .unavailable
                ? "Picture in Picture isn't ready yet."
                : nil
        )
    }

    var controlState: PictureInPictureControlState {
        Self.controlState(
            isSupported: isSupported,
            isActive: isActive,
            isPossible: isPossible,
            hasAttachedController: controller != nil
        )
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
                guard let self, self.controller === controller else { return }
                self.isPossible = controller.isPictureInPicturePossible
                if self.isPossible { self.clearErrorMessage() }
            }
        }
        activeObservation = controller.observe(\.isPictureInPictureActive,
                                               options: [.initial, .new]) { [weak self] controller, _ in
            Task { @MainActor in
                guard let self, self.controller === controller else { return }
                self.isActive = controller.isPictureInPictureActive
            }
        }
        #endif
    }

    func toggle() {
        clearErrorMessage()
        let state = controlState
        switch state.command {
        case .start:
            guard let controller else {
                showUnavailableMessage("Picture in Picture isn't ready yet.")
                return
            }
            controller.startPictureInPicture()
        case .stop:
            guard let controller else {
                showUnavailableMessage("Picture in Picture isn't ready yet.")
                return
            }
            controller.stopPictureInPicture()
        case .unavailable:
            showUnavailableMessage(state.messageOnTap)
        }
    }

    /// A tap while AVKit is not ready is useful feedback, not a playback
    /// failure. Give it the same bounded lifetime as the other nonfatal player
    /// notices so one retry cannot pin the banner and iOS system chrome.
    func showUnavailableMessage(_ message: String?, duration: Duration = .seconds(5)) {
        unavailableMessageTask?.cancel()
        unavailableMessageTask = nil
        errorMessage = message
        guard let message else { return }

        unavailableMessageTask = Task { [weak self] in
            do {
                try await Task.sleep(for: duration)
            } catch {
                return
            }
            guard let self, self.errorMessage == message else { return }
            self.errorMessage = nil
            self.unavailableMessageTask = nil
        }
    }

    /// AVKit's failure callback describes an attempted start, so unlike a
    /// not-yet-ready tap it remains until the player state changes.
    func showPersistentErrorMessage(_ message: String) {
        unavailableMessageTask?.cancel()
        unavailableMessageTask = nil
        errorMessage = message
    }

    private func clearErrorMessage() {
        unavailableMessageTask?.cancel()
        unavailableMessageTask = nil
        errorMessage = nil
    }

    private func stop() {
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
            if isPossible { isPossible = false }
            if isActive { isActive = false }
        }
    }

    func pictureInPictureControllerDidStartPictureInPicture(
        _ pictureInPictureController: AVPictureInPictureController
    ) {
        isActive = true
        clearErrorMessage()
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
        showPersistentErrorMessage(
            "Picture in Picture couldn't start: \(error.localizedDescription)"
        )
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
    let allowsPictureInPicture: Bool

    nonisolated static func shouldAllowPictureInPicture(
        isTearingDown: Bool,
        pgsOverlayIsActive: Bool
    ) -> Bool {
        !isTearingDown && !pgsOverlayIsActive
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(pictureInPicture: pictureInPicture)
    }

    func makeUIView(context: Context) -> PlayerSurfaceView {
        let view = PlayerSurfaceView()
        view.playerLayer.player = player
        view.applyPGSOverlay(pgsOverlay, to: player.currentItem)
        if allowsPictureInPicture {
            context.coordinator.pictureInPicture.attach(to: view.playerLayer)
        }
        return view
    }

    func updateUIView(_ view: PlayerSurfaceView, context: Context) {
        if view.playerLayer.player !== player {
            view.playerLayer.player = player
        }
        view.applyPGSOverlay(pgsOverlay, to: player.currentItem)
        if allowsPictureInPicture {
            context.coordinator.pictureInPicture.attach(to: view.playerLayer)
        } else {
            context.coordinator.pictureInPicture.detach()
        }
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
