#if os(iOS)
import Foundation
import PDFKit
import SwiftUI

enum PDFPageLocator {
    private static let prefix = "pdf/pages/"

    static func pageIndex(from locator: ReadingLocator?, pageCount: Int) -> Int? {
        guard pageCount > 0, let href = locator?.href,
              href.hasPrefix(prefix),
              let page = Int(href.dropFirst(prefix.count)),
              (1...pageCount).contains(page) else { return nil }
        return page - 1
    }

    static func progression(pageIndex: Int, pageCount: Int) -> Double {
        guard pageCount > 1 else { return 0 }
        return min(1, max(0, Double(pageIndex) / Double(pageCount - 1)))
    }

    static func locator(pageIndex: Int, pageCount: Int) -> ReadingLocator {
        let safePage = min(max(0, pageIndex), max(0, pageCount - 1))
        let progress = progression(pageIndex: safePage, pageCount: pageCount)
        return ReadingLocator(
            version: 1,
            href: "\(prefix)\(safePage + 1)",
            type: "application/pdf",
            title: "Page \(safePage + 1)",
            locations: ReadingLocations(
                progression: progress,
                totalProgression: progress,
                position: safePage + 1
            )
        )
    }
}

enum PDFReaderError: Error, LocalizedError {
    case invalidRevision
    case tooLarge
    case invalidResponse(Int)
    case incompleteDownload
    case invalidDocument
    case locked
    case accessibilityRestricted
    case empty

    var errorDescription: String? {
        switch self {
        case .invalidRevision:
            return "Cinema could not verify this PDF edition. Refresh the book and try again."
        case .tooLarge:
            return "This PDF is larger than Cinema's 1 GiB in-app safety limit. Use Open in… instead."
        case .invalidResponse(let status):
            return "The server returned HTTP \(status) while downloading this PDF."
        case .incompleteDownload:
            return "The downloaded PDF did not match the edition advertised by the server."
        case .invalidDocument:
            return "PDFKit could not open this document. It may be damaged or unsupported."
        case .locked:
            return "This PDF is password-protected. Cinema does not remove document protection."
        case .accessibilityRestricted:
            return "This PDF forbids accessibility access. Use Open in… with an appropriate PDF app."
        case .empty:
            return "This PDF does not contain any pages."
        }
    }
}

struct PDFReaderPayload {
    let document: PDFDocument
    let directory: URL

    func remove() {
        try? FileManager.default.removeItem(at: directory)
    }
}

enum PDFReaderLoader {
    static let maximumBytes = 1_073_741_824

    static func download(
        request: URLRequest,
        revision: ReadingRevision,
        session: URLSession
    ) async throws -> PDFReaderPayload {
        guard revision.size > 0 else { throw PDFReaderError.invalidRevision }
        guard revision.size <= maximumBytes else { throw PDFReaderError.tooLarge }

        let (temporary, response) = try await session.download(for: request)
        guard let http = response as? HTTPURLResponse,
              (200..<300).contains(http.statusCode) else {
            throw PDFReaderError.invalidResponse((response as? HTTPURLResponse)?.statusCode ?? 0)
        }

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("cinema-pdf-\(UUID().uuidString)", isDirectory: true)
        do {
            try FileManager.default.createDirectory(
                at: directory,
                withIntermediateDirectories: false,
                attributes: [.protectionKey: FileProtectionType.completeUnlessOpen]
            )
            let destination = directory.appendingPathComponent("document.pdf", isDirectory: false)
            try FileManager.default.moveItem(at: temporary, to: destination)
            let attributes = try FileManager.default.attributesOfItem(atPath: destination.path)
            let bytes = (attributes[.size] as? NSNumber)?.intValue ?? -1
            guard bytes == revision.size else { throw PDFReaderError.incompleteDownload }
            guard let document = PDFDocument(url: destination) else {
                throw PDFReaderError.invalidDocument
            }
            guard !document.isLocked else { throw PDFReaderError.locked }
            guard document.allowsContentAccessibility else {
                throw PDFReaderError.accessibilityRestricted
            }
            guard document.pageCount > 0 else { throw PDFReaderError.empty }
            return PDFReaderPayload(document: document, directory: directory)
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw error
        }
    }
}

enum PDFReaderTransport {
    static func session(origin: String) -> URLSession? {
        guard let originURL = URL(string: origin), permits(originURL) else { return nil }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.waitsForConnectivity = true
        configuration.timeoutIntervalForRequest = 90
        configuration.timeoutIntervalForResource = 60 * 60
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        let delegate = PDFSameOriginRedirectDelegate(origin: originURL)
        return URLSession(configuration: configuration, delegate: delegate, delegateQueue: nil)
    }

    static func permitsRedirect(from origin: URL, to candidate: URL) -> Bool {
        guard permits(origin), permits(candidate) else { return false }
        return origin.scheme?.lowercased() == candidate.scheme?.lowercased()
            && origin.host?.lowercased() == candidate.host?.lowercased()
            && effectivePort(origin) == effectivePort(candidate)
    }

    private static func permits(_ url: URL) -> Bool {
        ["http", "https"].contains(url.scheme?.lowercased() ?? "")
            && url.host?.isEmpty == false
    }

    private static func effectivePort(_ url: URL) -> Int? {
        if let port = url.port { return port }
        return url.scheme?.lowercased() == "https" ? 443 : 80
    }
}

private final class PDFSameOriginRedirectDelegate: NSObject, URLSessionTaskDelegate {
    private let origin: URL

    init(origin: URL) {
        self.origin = origin
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        guard let redirectURL = request.url,
              PDFReaderTransport.permitsRedirect(from: origin, to: redirectURL) else {
            completionHandler(nil)
            return
        }
        completionHandler(request)
    }
}

struct PDFReaderView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    let context: ReaderContext

    @State private var payload: PDFReaderPayload?
    @State private var currentPage = 0
    @State private var requestedPage = 0
    @State private var searchText = ""
    @State private var searchResults: [PDFSelection] = []
    @State private var selectedSearchResult = 0
    @State private var errorMessage: String?
    @State private var loading = true
    @State private var closing = false
    @State private var generation = 0
    @State private var saveTask: Task<Void, Never>?

    var body: some View {
        NavigationStack {
            Group {
                if let payload {
                    PDFReaderCanvas(
                        document: payload.document,
                        requestedPage: requestedPage,
                        selection: selectedSelection,
                        onPageChanged: pageChanged
                    )
                    .ignoresSafeArea(edges: .bottom)
                    .safeAreaInset(edge: .bottom) { pageBar(document: payload.document) }
                    .safeAreaInset(edge: .top) { searchBar }
                } else if loading {
                    ProgressView("Opening PDF…")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    ContentUnavailableView {
                        Label("Couldn't open this PDF", systemImage: "doc.richtext")
                    } description: {
                        Text(errorMessage ?? "Cinema could not open this document.")
                    } actions: {
                        Button("Try Again") { generation += 1 }
                            .buttonStyle(.borderedProminent)
                    }
                }
            }
            .background(Palette.bg.ignoresSafeArea())
            .navigationTitle(context.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button("Close") { close(completed: false) }
                        .disabled(closing)
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        close(completed: true)
                    } label: {
                        Label("Mark finished", systemImage: "checkmark.circle")
                    }
                    .disabled(payload == nil || closing)
                    .accessibilityHint("Saves this book as finished and closes the reader")
                }
            }
        }
        .interactiveDismissDisabled()
        .task(id: generation) { await load() }
        .onChange(of: searchText) { _, query in search(query) }
        .onChange(of: scenePhase) { _, phase in
            if phase != .active { persistCurrentPage() }
        }
        .onChange(of: model.phase) { _, phase in
            if phase != .ready { close(completed: false) }
        }
        .onDisappear {
            saveTask?.cancel()
            payload?.remove()
            payload = nil
        }
    }

    private var selectedSelection: PDFSelection? {
        guard searchResults.indices.contains(selectedSearchResult) else { return nil }
        return searchResults[selectedSearchResult]
    }

    @ViewBuilder
    private var searchBar: some View {
        if payload != nil {
            HStack(spacing: 10) {
                Image(systemName: "magnifyingglass")
                    .foregroundStyle(.secondary)
                TextField("Search PDF", text: $searchText)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                if !searchText.isEmpty {
                    if searchResults.isEmpty {
                        Text("No matches")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        Text("\(selectedSearchResult + 1) of \(searchResults.count)")
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                        Button { selectSearchResult(selectedSearchResult - 1) } label: {
                            Image(systemName: "chevron.up")
                        }
                        .accessibilityLabel("Previous match")
                        Button { selectSearchResult(selectedSearchResult + 1) } label: {
                            Image(systemName: "chevron.down")
                        }
                        .accessibilityLabel("Next match")
                    }
                    Button {
                        searchText = ""
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                    }
                    .foregroundStyle(.secondary)
                    .accessibilityLabel("Clear search")
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(.bar)
        }
    }

    private func pageBar(document: PDFDocument) -> some View {
        HStack(spacing: 18) {
            Button {
                requestedPage = max(0, currentPage - 1)
            } label: {
                Image(systemName: "chevron.left")
            }
            .disabled(currentPage == 0)
            .accessibilityLabel("Previous page")

            Text("Page \(currentPage + 1) of \(document.pageCount)")
                .font(.subheadline.monospacedDigit())
                .frame(minWidth: 130)

            Button {
                requestedPage = min(document.pageCount - 1, currentPage + 1)
            } label: {
                Image(systemName: "chevron.right")
            }
            .disabled(currentPage >= document.pageCount - 1)
            .accessibilityLabel("Next page")
        }
        .buttonStyle(.bordered)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity)
        .background(.bar)
    }

    private func load() async {
        saveTask?.cancel()
        payload?.remove()
        payload = nil
        errorMessage = nil
        loading = true
        searchText = ""
        searchResults = []

        guard let revision = context.revision else {
            errorMessage = PDFReaderError.invalidRevision.localizedDescription
            loading = false
            return
        }
        let api = PlurxAPI(origin: model.origin)
        guard let session = PDFReaderTransport.session(origin: model.origin) else {
            errorMessage = APIError.badURL.localizedDescription
            loading = false
            return
        }
        defer { session.finishTasksAndInvalidate() }

        do {
            let saved = try? await api.readingState(itemId: context.itemId, fileId: context.fileId)
            let request = try api.bookContentRequest(fileId: context.fileId, accept: "application/pdf")
            let loaded = try await PDFReaderLoader.download(
                request: request,
                revision: revision,
                session: session
            )
            guard !Task.isCancelled else {
                loaded.remove()
                return
            }
            let state = saved?.state
            let initial = state?.revision == revision && state?.completed != true
                ? PDFPageLocator.pageIndex(from: state?.locator, pageCount: loaded.document.pageCount) ?? 0
                : 0
            payload = loaded
            currentPage = initial
            requestedPage = initial
            loading = false
        } catch is CancellationError {
            return
        } catch {
            errorMessage = error.localizedDescription
            loading = false
        }
    }

    private func pageChanged(_ page: Int) {
        guard page != currentPage || requestedPage != page else { return }
        currentPage = page
        requestedPage = page
        scheduleSave(page: page)
    }

    private func scheduleSave(page: Int) {
        saveTask?.cancel()
        saveTask = Task {
            try? await Task.sleep(for: .milliseconds(350))
            guard !Task.isCancelled else { return }
            await save(page: page, completed: false)
        }
    }

    private func persistCurrentPage() {
        guard payload != nil else { return }
        saveTask?.cancel()
        saveTask = Task { await save(page: currentPage, completed: false) }
    }

    private func save(page: Int, completed: Bool) async {
        guard let revision = context.revision, let pageCount = payload?.document.pageCount else { return }
        let progression = PDFPageLocator.progression(pageIndex: page, pageCount: pageCount)
        let state = PutReadingStateRequest(
            fileId: context.fileId,
            revision: revision,
            locator: PDFPageLocator.locator(pageIndex: page, pageCount: pageCount),
            progression: progression,
            completed: completed
        )
        _ = try? await PlurxAPI(origin: model.origin).putReadingState(itemId: context.itemId, state: state)
    }

    private func close(completed: Bool) {
        guard !closing else { return }
        closing = true
        saveTask?.cancel()
        Task {
            if payload != nil { await save(page: currentPage, completed: completed) }
            dismiss()
        }
    }

    private func search(_ query: String) {
        let normalized = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !normalized.isEmpty, let document = payload?.document else {
            searchResults = []
            selectedSearchResult = 0
            return
        }
        searchResults = document.findString(normalized, withOptions: .caseInsensitive)
        selectedSearchResult = 0
    }

    private func selectSearchResult(_ index: Int) {
        guard !searchResults.isEmpty else { return }
        selectedSearchResult = (index + searchResults.count) % searchResults.count
    }
}

private struct PDFReaderCanvas: UIViewRepresentable {
    let document: PDFDocument
    let requestedPage: Int
    let selection: PDFSelection?
    let onPageChanged: (Int) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(onPageChanged: onPageChanged) }

    func makeUIView(context: Context) -> PDFView {
        let view = PDFView()
        view.document = document
        view.displayMode = .singlePageContinuous
        view.displayDirection = .vertical
        view.displaysPageBreaks = true
        view.autoScales = true
        view.delegate = context.coordinator
        context.coordinator.observe(view)
        context.coordinator.apply(page: requestedPage, selection: selection, to: view)
        return view
    }

    func updateUIView(_ view: PDFView, context: Context) {
        if view.document !== document { view.document = document }
        context.coordinator.onPageChanged = onPageChanged
        context.coordinator.apply(page: requestedPage, selection: selection, to: view)
    }

    static func dismantleUIView(_ view: PDFView, coordinator: Coordinator) {
        coordinator.stopObserving()
        view.delegate = nil
        view.document = nil
    }

    final class Coordinator: NSObject, PDFViewDelegate {
        var onPageChanged: (Int) -> Void
        private weak var view: PDFView?
        private var pageObserver: NSObjectProtocol?
        private weak var appliedSelection: PDFSelection?

        init(onPageChanged: @escaping (Int) -> Void) {
            self.onPageChanged = onPageChanged
        }

        func observe(_ view: PDFView) {
            self.view = view
            pageObserver = NotificationCenter.default.addObserver(
                forName: .PDFViewPageChanged,
                object: view,
                queue: .main
            ) { [weak self] _ in
                guard let self, let view = self.view,
                      let document = view.document, let page = view.currentPage else { return }
                self.onPageChanged(document.index(for: page))
            }
        }

        func stopObserving() {
            if let pageObserver { NotificationCenter.default.removeObserver(pageObserver) }
            pageObserver = nil
        }

        func apply(page: Int, selection: PDFSelection?, to view: PDFView) {
            if let target = documentPage(page, in: view), view.currentPage !== target {
                view.go(to: target)
            }
            guard selection !== appliedSelection else { return }
            appliedSelection = selection
            view.highlightedSelections = selection.map { [$0] }
            view.setCurrentSelection(selection, animate: true)
            if let selection { view.go(to: selection) }
        }

        private func documentPage(_ index: Int, in view: PDFView) -> PDFPage? {
            guard let document = view.document, document.pageCount > 0 else { return nil }
            return document.page(at: min(max(0, index), document.pageCount - 1))
        }

        // PDF links and remote-document actions never escape the reader
        // implicitly. The user can deliberately export the original from the
        // detail screen's Open in… action.
        func pdfViewWillClick(onLink sender: PDFView, with url: URL) {}
        func pdfViewOpenPDF(_ sender: PDFView, forRemoteGoToAction action: PDFActionRemoteGoTo) {}

        deinit { stopObserving() }
    }
}
#endif
