#if os(iOS)
import UIKit

final class OfflineAppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        handleEventsForBackgroundURLSession identifier: String,
        completionHandler: @escaping () -> Void
    ) {
        OfflineDownloadManager.shared.handleEvents(
            forBackgroundURLSession: identifier,
            completion: completionHandler
        )
    }
}
#endif
