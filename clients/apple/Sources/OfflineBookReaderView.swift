#if os(iOS)
import Foundation
import SwiftUI
import WebKit

enum OfflineBookResourceResolver {
    static let scheme = "cinema-book"
    static let host = "offline"

    static func publicationPath(for url: URL) -> String? {
        guard url.scheme == scheme, url.host == host,
              url.path.hasPrefix("/publication/") else { return nil }
        let suffix = String(url.path.dropFirst("/publication/".count))
        return OfflineBookManager.safePublicationPath(suffix)
    }

    static func mimeType(for path: String, publication: PublicationManifest) -> String {
        let links = publication.readingOrder + publication.resources
        return links.first {
            OfflineBookManager.safePublicationPath($0.href) == path
        }?.type ?? "application/octet-stream"
    }
}

enum OfflineBookNetworkPolicy {
    /// EPUB subresources are untrusted publisher content. The reader itself is
    /// custom-scheme-only, so every ordinary network scheme can be blocked at
    /// WebKit's resource-loader boundary, including CSS imports and images.
    static let contentRuleList = #"[{"trigger":{"url-filter":"^https?://"},"action":{"type":"block"}}]"#
}

private final class OfflineBookSchemeHandler: NSObject, WKURLSchemeHandler {
    private let publication: PublicationManifest
    private let root: URL
    private let queue: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "tv.plurx.offline-book-scheme"
        queue.maxConcurrentOperationCount = 2
        return queue
    }()
    private let lock = NSLock()
    private var cancelled: Set<ObjectIdentifier> = []

    init?(book: OfflineBook) {
        guard let publication = book.publication,
              let path = book.localPublicationRelativePath else { return nil }
        self.publication = publication
        self.root = OfflineCatalog.localURL(for: path)
        super.init()
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        let identifier = ObjectIdentifier(urlSchemeTask as AnyObject)
        queue.addOperation { [weak self, weak task = urlSchemeTask as AnyObject] in
            guard let self else { return }
            guard let task = task as? WKURLSchemeTask,
                  !self.isCancelled(identifier) else {
                self.clear(identifier)
                return
            }
            do {
                let (file, mime) = try self.resolve(task.request.url)
                let attributes = try FileManager.default.attributesOfItem(atPath: file.path)
                let length = (attributes[.size] as? NSNumber)?.intValue ?? -1
                guard let url = task.request.url else { throw URLError(.badURL) }
                task.didReceive(URLResponse(
                    url: url,
                    mimeType: mime,
                    expectedContentLength: length,
                    textEncodingName: Self.isText(mime) ? "utf-8" : nil
                ))
                let handle = try FileHandle(forReadingFrom: file)
                defer { try? handle.close() }
                while !self.isCancelled(identifier) {
                    let data = try handle.read(upToCount: 64 * 1_024) ?? Data()
                    if data.isEmpty { break }
                    task.didReceive(data)
                }
                if !self.isCancelled(identifier) { task.didFinish() }
            } catch {
                if !self.isCancelled(identifier) { task.didFailWithError(error) }
            }
            self.clear(identifier)
        }
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        lock.lock()
        cancelled.insert(ObjectIdentifier(urlSchemeTask as AnyObject))
        lock.unlock()
    }

    private func resolve(_ url: URL?) throws -> (URL, String) {
        guard let url, url.scheme == OfflineBookResourceResolver.scheme,
              url.host == OfflineBookResourceResolver.host else { throw URLError(.badURL) }
        switch url.path {
        case "/offline-reader.html":
            return try bundled("offline-reader", extension: "html", mime: "text/html")
        case "/reader.js":
            return try bundled("reader", extension: "js", mime: "text/javascript")
        case "/offline-reader.js":
            return try bundled("offline-reader", extension: "js", mime: "text/javascript")
        default:
            guard let path = OfflineBookResourceResolver.publicationPath(for: url) else {
                throw URLError(.fileDoesNotExist)
            }
            let base = root.appendingPathComponent("publication", isDirectory: true)
            let file = base.appendingPathComponent(path).standardizedFileURL
            guard file.pathComponents.starts(with: base.standardizedFileURL.pathComponents),
                  FileManager.default.fileExists(atPath: file.path) else {
                throw URLError(.fileDoesNotExist)
            }
            return (file, OfflineBookResourceResolver.mimeType(for: path, publication: publication))
        }
    }

    private func bundled(_ name: String, extension ext: String, mime: String) throws -> (URL, String) {
        guard let url = Bundle.main.url(forResource: name, withExtension: ext) else {
            throw URLError(.fileDoesNotExist)
        }
        return (url, mime)
    }

    private func isCancelled(_ identifier: ObjectIdentifier) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return cancelled.contains(identifier)
    }

    private func clear(_ identifier: ObjectIdentifier) {
        lock.lock(); cancelled.remove(identifier); lock.unlock()
    }

    private static func isText(_ mime: String) -> Bool {
        mime.hasPrefix("text/") || mime.contains("xml") || mime.contains("javascript")
            || mime.contains("json") || mime.contains("svg")
    }
}

struct OfflineBookReaderView: View {
    @Environment(\.dismiss) private var dismiss
    let book: OfflineBook
    @State private var errorMessage: String?

    var body: some View {
        ZStack {
            Palette.bg.ignoresSafeArea()
            OfflineBookWebView(
                book: book,
                onClose: { dismiss() },
                onError: { errorMessage = $0 }
            )
            .ignoresSafeArea()
            if let errorMessage {
                ContentUnavailableView {
                    Label("Couldn't open this download", systemImage: "book.closed")
                } description: {
                    Text(errorMessage)
                } actions: {
                    Button("Close") { dismiss() }.buttonStyle(.borderedProminent)
                }
                .padding(24).background(.ultraThinMaterial)
            }
        }
    }
}

private struct OfflineReaderPayload: Encodable {
    let publication: PublicationManifest
    let limits: PublicationLimits?
    let resourceBase: String
    let locator: ReadingLocator?
    let progression: Double
    let completed: Bool
    let preferences: OfflineBookPreferences
}

private struct OfflineBookWebView: UIViewRepresentable {
    let book: OfflineBook
    let onClose: () -> Void
    let onError: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    func makeUIView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        guard let schemeHandler = OfflineBookSchemeHandler(book: book) else {
            DispatchQueue.main.async { onError("This local EPUB is incomplete. Download it again.") }
            return WKWebView(frame: .zero, configuration: configuration)
        }
        configuration.setURLSchemeHandler(
            schemeHandler,
            forURLScheme: OfflineBookResourceResolver.scheme
        )
        configuration.userContentController.add(context.coordinator, name: "cinemaOfflineReader")
        context.coordinator.schemeHandler = schemeHandler

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.isOpaque = false
        webView.backgroundColor = .clear
        webView.scrollView.backgroundColor = .clear
        WKContentRuleListStore.default().compileContentRuleList(
            forIdentifier: "tv.plurx.offline-book-network-v1",
            encodedContentRuleList: OfflineBookNetworkPolicy.contentRuleList
        ) { [weak webView, weak coordinator = context.coordinator] ruleList, error in
            DispatchQueue.main.async {
                guard let webView, let coordinator, !coordinator.tornDown else { return }
                guard let ruleList else {
                    onError(error?.localizedDescription ?? "Cinema could not secure the offline reader.")
                    return
                }
                configuration.userContentController.add(ruleList)
                webView.load(URLRequest(
                    url: URL(string: "cinema-book://offline/offline-reader.html")!
                ))
            }
        }
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static func dismantleUIView(_ webView: WKWebView, coordinator: Coordinator) {
        coordinator.tornDown = true
        webView.evaluateJavaScript("window.dispatchEvent(new Event('pagehide'));void 0")
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.uiDelegate = nil
        webView.configuration.userContentController.removeScriptMessageHandler(
            forName: "cinemaOfflineReader"
        )
        webView.loadHTMLString("", baseURL: nil)
        coordinator.schemeHandler = nil
    }

    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
        private let parent: OfflineBookWebView
        fileprivate var schemeHandler: OfflineBookSchemeHandler?
        fileprivate var tornDown = false
        private var started = false

        init(parent: OfflineBookWebView) { self.parent = parent }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            guard !started, let publication = parent.book.publication else { return }
            do {
                let payload = OfflineReaderPayload(
                    publication: publication,
                    limits: parent.book.limits,
                    resourceBase: "cinema-book://offline/publication/",
                    locator: parent.book.locator,
                    progression: parent.book.progression,
                    completed: parent.book.completed,
                    preferences: parent.book.preferences
                )
                let data = try JSONEncoder().encode(payload)
                guard let json = String(data: data, encoding: .utf8) else {
                    throw APIError.transport("Cinema could not encode the local reader state.")
                }
                started = true
                webView.evaluateJavaScript("window.startOfflineReader(\(json));") { _, error in
                    if let error { self.parent.onError(error.localizedDescription) }
                }
            } catch { parent.onError(error.localizedDescription) }
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
        ) {
            guard let url = navigationAction.request.url else {
                decisionHandler(.cancel); return
            }
            let allowed = url.absoluteString == "about:blank"
                || (url.scheme == OfflineBookResourceResolver.scheme
                    && url.host == OfflineBookResourceResolver.host)
            decisionHandler(allowed ? .allow : .cancel)
        }

        func webView(
            _ webView: WKWebView,
            didFailProvisionalNavigation navigation: WKNavigation!,
            withError error: Error
        ) { parent.onError(error.localizedDescription) }

        func webView(
            _ webView: WKWebView,
            createWebViewWith configuration: WKWebViewConfiguration,
            for navigationAction: WKNavigationAction,
            windowFeatures: WKWindowFeatures
        ) -> WKWebView? { nil }

        func userContentController(
            _ userContentController: WKUserContentController,
            didReceive message: WKScriptMessage
        ) {
            guard message.frameInfo.isMainFrame,
                  let payload = message.body as? [String: Any],
                  let event = payload["event"] as? String else { return }
            switch event {
            case "close": parent.onClose()
            case "error": parent.onError(payload["message"] as? String ?? "Cinema could not open this EPUB.")
            case "progress": record(payload)
            default: break
            }
        }

        private func record(_ payload: [String: Any]) {
            guard let locator = payload["locator"],
                  JSONSerialization.isValidJSONObject(locator),
                  let progression = payload["progression"] as? NSNumber,
                  let completed = payload["completed"] as? Bool,
                  let recordedAt = payload["recorded_at"] as? NSNumber,
                  let preferences = payload["preferences"],
                  JSONSerialization.isValidJSONObject(preferences) else { return }
            do {
                let locatorData = try JSONSerialization.data(withJSONObject: locator)
                let preferencesData = try JSONSerialization.data(withJSONObject: preferences)
                let decodedLocator = try JSONDecoder().decode(ReadingLocator.self, from: locatorData)
                let decodedPreferences = try JSONDecoder().decode(
                    OfflineBookPreferences.self,
                    from: preferencesData
                )
                Task {
                    await OfflineBookManager.shared.record(
                        bookId: parent.book.id,
                        locator: decodedLocator,
                        progression: progression.doubleValue,
                        completed: completed,
                        recordedAt: recordedAt.intValue,
                        preferences: decodedPreferences
                    )
                }
            } catch { parent.onError("Cinema could not save this reading position.") }
        }
    }
}
#endif
