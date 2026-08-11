import SwiftUI
#if os(iOS)
import Vision
import VisionKit
#endif

enum ConnectionCode {
    static func origin(from payload: String) -> String? {
        var candidate = payload.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !candidate.isEmpty else { return nil }

        if let code = URLComponents(string: candidate), code.scheme?.lowercased() == "plurx" {
            guard code.host?.lowercased() == "connect",
                  let encodedOrigin = code.queryItems?.first(where: {
                    ["origin", "server", "address"].contains($0.name.lowercased())
                  })?.value else { return nil }
            candidate = encodedOrigin
        }

        let normalized = AppModel.normalizeOrigin(candidate)
        guard let url = URLComponents(string: normalized),
              ["http", "https"].contains(url.scheme?.lowercased() ?? ""),
              url.host?.isEmpty == false,
              url.user == nil,
              url.password == nil,
              url.path.isEmpty else { return nil }
        return normalized
    }
}

/// Filled primary action with an inline spinner while `busy`.
struct PrimaryButton: View {
    let title: String
    var busy: Bool = false
    var disabled: Bool = false
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            ZStack {
                if busy { ProgressView().tint(.white) }
                Text(title).fontWeight(.semibold).opacity(busy ? 0 : 1)
            }
        }
        #if os(tvOS)
        .buttonStyle(TVReadableButtonStyle(prominent: true))
        #else
        .buttonStyle(IOSFullWidthActionButtonStyle(prominent: true))
        #endif
        .disabled(disabled || busy)
    }
}

private struct AuthScaffold<Content: View>: View {
    let subtitle: String
    let error: String?
    @ViewBuilder var content: Content

    var body: some View {
        VStack(spacing: 16) {
            Text("cinema")
                .font(.system(size: 44, weight: .bold, design: .monospaced))
                .foregroundColor(Palette.accent)
            Text(subtitle)
                .font(.system(.callout, design: .monospaced))
                .foregroundColor(Palette.muted)
            content
            if let error {
                Text(error)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(Palette.accent)
                    .multilineTextAlignment(.center)
            }
        }
        .frame(maxWidth: 460)
        .padding(30)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct ConnectView: View {
    @EnvironmentObject var model: AppModel
    @ObservedObject var discovery: ServerDiscovery
    @State private var url = ""
    @State private var showManual = false
    @State private var resolving: String?
    #if os(iOS)
    @State private var showQRScanner = false
    @State private var qrError: String?
    #endif

    var body: some View {
        AuthScaffold(subtitle: "Servers on your network", error: model.authError) {
            if discovery.servers.isEmpty {
                HStack(spacing: 10) {
                    if discovery.isSearching { ProgressView().tint(Palette.accent) }
                    Text(discovery.isSearching ? "Looking for Cinema…" : "No servers found")
                        .font(.system(.callout, design: .monospaced))
                        .foregroundColor(Palette.muted)
                }
                .padding(.vertical, 8)
            } else {
                VStack(spacing: 10) {
                    ForEach(discovery.servers) { server in
                        Button { choose(server) } label: {
                            HStack(spacing: 12) {
                                Image(systemName: "externaldrive.connected.to.line.below")
                                    .foregroundColor(Palette.accent)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(server.name).fontWeight(.semibold)
                                    Text("Cinema server")
                                        .font(.caption)
                                        .foregroundColor(Palette.muted)
                                }
                                Spacer()
                                if resolving == server.id {
                                    ProgressView().tint(Palette.accent)
                                } else {
                                    Image(systemName: "chevron.right")
                                        .foregroundColor(Palette.muted)
                                }
                            }
                            .font(.system(.body, design: .monospaced))
                            .padding(14)
                            .background(Palette.surface, in: RoundedRectangle(cornerRadius: 10))
                        }
                        .buttonStyle(.plain)
                        .disabled(model.busy || resolving != nil)
                    }
                }
            }

            Button {
                withAnimation { showManual.toggle() }
            } label: {
                Label(showManual ? "Hide manual setup" : "Add server manually",
                      systemImage: showManual ? "chevron.up" : "plus")
            }
            .font(.system(.callout, design: .monospaced))
            .buttonStyle(.plain)
            .foregroundColor(Palette.muted)

            #if os(iOS)
            if DataScannerViewController.isSupported {
                Button {
                    if DataScannerViewController.isAvailable {
                        qrError = nil
                        showQRScanner = true
                    } else {
                        qrError = "Camera scanning is unavailable. Check Camera access in Settings."
                    }
                } label: {
                    Label("Scan server QR code", systemImage: "qrcode.viewfinder")
                }
                .font(.system(.callout, design: .monospaced))
                .buttonStyle(.bordered)
                .tint(Palette.accent)
                .disabled(model.busy || resolving != nil)
            }

            if let qrError {
                Text(qrError)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundColor(Palette.accent)
                    .multilineTextAlignment(.center)
            }
            #endif

            if showManual {
                TextField("192.168.1.10:32400", text: $url)
                    .plurxFieldStyle()
                    .font(.system(.body, design: .monospaced))
                    #if os(iOS)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    #endif
                    .onSubmit { connect() }

                PrimaryButton(title: "Connect", busy: model.busy, disabled: url.isEmpty) { connect() }

                Text("Enter a hostname or address. A bare host uses port 32400.")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundColor(Palette.muted)
                    .multilineTextAlignment(.center)
            }

            if let message = discovery.errorMessage {
                Text(message)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundColor(Palette.muted)
                    .multilineTextAlignment(.center)
            }

            Button("Scan again") { discovery.restart() }
                .font(.system(.caption, design: .monospaced))
                .buttonStyle(.plain)
                .foregroundColor(Palette.muted)
        }
        .onAppear {
            discovery.start()
            if url.isEmpty { url = model.origin }
        }
        #if os(iOS)
        .sheet(isPresented: $showQRScanner) {
            QRCodeScannerView { payload in
                showQRScanner = false
                guard let scannedOrigin = ConnectionCode.origin(from: payload) else {
                    qrError = "That QR code doesn't contain a valid Cinema server address."
                    return
                }
                qrError = nil
                url = scannedOrigin
                showManual = true
                Task { await model.connect(scannedOrigin) }
            }
            .ignoresSafeArea()
        }
        #endif
    }

    private func connect() { Task { await model.connect(url) } }

    private func choose(_ server: DiscoveredServer) {
        resolving = server.id
        Task {
            do {
                let origin = try await discovery.resolve(server)
                resolving = nil
                await model.connect(origin)
            } catch {
                resolving = nil
                // docs/CLIENT-CONNECTIVITY.md §5: the inline field shape. A
                // Bonjour resolution failure keeps `ServerDiscoveryError`'s own
                // sentence; anything the classifier can place renders its
                // class. Neither is Foundation's wording.
                model.authError = Connectivity.message(for: error, server: server.name)
            }
        }
    }
}

#if os(iOS)
private struct QRCodeScannerView: UIViewControllerRepresentable {
    let onScanned: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(onScanned: onScanned) }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: true,
            isPinchToZoomEnabled: true,
            isGuidanceEnabled: true,
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        DispatchQueue.main.async { try? scanner.startScanning() }
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {}

    static func dismantleUIViewController(
        _ scanner: DataScannerViewController,
        coordinator: Coordinator
    ) {
        scanner.stopScanning()
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onScanned: (String) -> Void
        private var finished = false

        init(onScanned: @escaping (String) -> Void) {
            self.onScanned = onScanned
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            guard let item = addedItems.first else { return }
            finish(item, using: dataScanner)
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            didTapOn item: RecognizedItem
        ) {
            finish(item, using: dataScanner)
        }

        private func finish(
            _ item: RecognizedItem,
            using dataScanner: DataScannerViewController
        ) {
            guard !finished,
                  case .barcode(let barcode) = item,
                  let payload = barcode.payloadStringValue else { return }
            finished = true
            dataScanner.stopScanning()
            onScanned(payload)
        }
    }
}
#endif

struct LoginView: View {
    @EnvironmentObject var model: AppModel
    @State private var username = ""
    @State private var password = ""

    var body: some View {
        AuthScaffold(subtitle: model.serverName ?? model.origin, error: model.authError) {
            TextField("Username", text: $username)
                .plurxFieldStyle()
                .font(.system(.body, design: .monospaced))
                #if os(iOS)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                #endif

            SecureField("Password", text: $password)
                .plurxFieldStyle()
                .font(.system(.body, design: .monospaced))
                .onSubmit { signIn() }

            PrimaryButton(title: "Sign in", busy: model.busy, disabled: username.isEmpty || password.isEmpty) { signIn() }

            Button("Use a different server") { model.changeServer() }
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(Palette.muted)
                .buttonStyle(.plain)
        }
        .onAppear { if username.isEmpty { username = model.username ?? "" } }
    }

    private func signIn() { Task { await model.login(username, password) } }
}

/// A saved token is still valid until the server explicitly rejects it. A
/// timeout, unavailable route, or local-network permission problem belongs on
/// a reconnect screen—not on a username/password form.
struct ReconnectView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        AuthScaffold(subtitle: model.serverName ?? model.origin, error: model.authError) {
            Text("Your sign-in is still saved.")
                .font(.system(.body, design: .rounded))
                .foregroundColor(Palette.onBg.opacity(0.82))

            PrimaryButton(title: "Try again", busy: model.busy) {
                Task { await model.retrySavedSession() }
            }

            Button("Use a different server") { model.changeServer() }
                .font(.system(.caption, design: .monospaced))
                .foregroundColor(Palette.muted)
                .buttonStyle(.plain)
        }
    }
}
