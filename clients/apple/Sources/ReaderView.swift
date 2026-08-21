#if os(iOS)
import Foundation
import SwiftUI
import WebKit

enum NativeReaderHandoff {
    static func shellURL(origin: String) -> URL? {
        guard var components = URLComponents(string: origin) else { return nil }
        guard components.scheme?.lowercased() == "http"
                || components.scheme?.lowercased() == "https",
              components.host?.isEmpty == false else { return nil }
        components.path = "/"
        components.queryItems = [URLQueryItem(name: "native-reader", value: "1")]
        components.fragment = nil
        return components.url
    }

    static func startScript(token: String, itemId: Int, fileId: Int) -> String? {
        guard !token.isEmpty, itemId > 0, fileId > 0,
              let data = try? JSONEncoder().encode(token),
              let encodedToken = String(data: data, encoding: .utf8) else { return nil }
        return "window.startNativeReader(\(encodedToken),\(itemId),\(fileId));"
    }

    static func permitsNavigation(_ candidate: URL, from origin: URL) -> Bool {
        if candidate.scheme == "about" { return candidate.absoluteString == "about:blank" }
        guard candidate.scheme?.lowercased() == origin.scheme?.lowercased(),
              candidate.host?.lowercased() == origin.host?.lowercased(),
              effectivePort(candidate) == effectivePort(origin) else { return false }
        return candidate.path == "/"
            || candidate.path.hasPrefix("/assets/")
            || candidate.path.hasPrefix("/api/v1/publication/")
    }

    private static func effectivePort(_ url: URL) -> Int? {
        if let port = url.port { return port }
        switch url.scheme?.lowercased() {
        case "http": return 80
        case "https": return 443
        default: return nil
        }
    }
}

struct ReaderView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let context: ReaderContext
    @State private var errorMessage: String?

    var body: some View {
        ZStack {
            Palette.bg.ignoresSafeArea()
            ReaderWebView(
                context: context,
                origin: model.origin,
                onEvent: handleEvent,
                onError: { errorMessage = $0 }
            )
            .ignoresSafeArea()

            if let errorMessage {
                ContentUnavailableView {
                    Label("Couldn't open this book", systemImage: "book.closed")
                } description: {
                    Text(errorMessage)
                } actions: {
                    Button("Close") { dismiss() }
                        .buttonStyle(.borderedProminent)
                }
                .padding(24)
                .background(.ultraThinMaterial)
            }
        }
        .onChange(of: model.phase) { _, phase in
            if phase != .ready { dismiss() }
        }
    }

    private func handleEvent(_ event: String, message: String?) {
        switch event {
        case "close", "session-ended":
            dismiss()
        case "ready":
            errorMessage = nil
        case "error":
            errorMessage = message ?? "Cinema could not open this EPUB."
        default:
            break
        }
    }
}

private struct ReaderWebView: UIViewRepresentable {
    let context: ReaderContext
    let origin: String
    let onEvent: (String, String?) -> Void
    let onError: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    func makeUIView(context coordinatorContext: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.userContentController.add(coordinatorContext.coordinator, name: "cinemaReader")

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = coordinatorContext.coordinator
        webView.uiDelegate = coordinatorContext.coordinator
        webView.isOpaque = false
        webView.backgroundColor = .clear
        webView.scrollView.backgroundColor = .clear
        #if DEBUG
        if #available(iOS 16.4, *) { webView.isInspectable = true }
        #endif

        guard let url = NativeReaderHandoff.shellURL(origin: origin) else {
            onError("The saved Cinema server address is invalid.")
            return webView
        }
        coordinatorContext.coordinator.shellOrigin = url
        webView.load(URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData))
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static func dismantleUIView(_ webView: WKWebView, coordinator: Coordinator) {
        webView.evaluateJavaScript(
            "if(typeof READER!=='undefined'&&READER&&typeof destroyReader==='function')destroyReader(true);TOKEN=null;ME=null;"
        )
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.uiDelegate = nil
        webView.configuration.userContentController.removeScriptMessageHandler(forName: "cinemaReader")
        webView.loadHTMLString("", baseURL: nil)
    }

    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
        private let parent: ReaderWebView
        fileprivate var shellOrigin: URL?
        private var started = false

        init(parent: ReaderWebView) { self.parent = parent }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            guard !started else { return }
            guard let token = Session.shared.token,
                  Session.shared.origin == parent.origin,
                  let script = NativeReaderHandoff.startScript(
                    token: token,
                    itemId: parent.context.itemId,
                    fileId: parent.context.fileId
                  ) else {
                parent.onEvent("session-ended", nil)
                return
            }
            started = true
            webView.evaluateJavaScript(script) { _, error in
                if let error { self.parent.onError(error.localizedDescription) }
            }
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
        ) {
            guard let candidate = navigationAction.request.url,
                  let shellOrigin,
                  NativeReaderHandoff.permitsNavigation(candidate, from: shellOrigin) else {
                decisionHandler(.cancel)
                return
            }
            decisionHandler(.allow)
        }

        func webView(
            _ webView: WKWebView,
            didFail navigation: WKNavigation!,
            withError error: Error
        ) {
            parent.onError(error.localizedDescription)
        }

        func webView(
            _ webView: WKWebView,
            didFailProvisionalNavigation navigation: WKNavigation!,
            withError error: Error
        ) {
            parent.onError(error.localizedDescription)
        }

        func webView(
            _ webView: WKWebView,
            createWebViewWith configuration: WKWebViewConfiguration,
            for navigationAction: WKNavigationAction,
            windowFeatures: WKWindowFeatures
        ) -> WKWebView? { nil }

        func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage) {
            guard message.frameInfo.isMainFrame,
                  let payload = message.body as? [String: Any],
                  let event = payload["event"] as? String else { return }
            parent.onEvent(event, payload["message"] as? String)
        }
    }
}
#endif
