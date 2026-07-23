import SwiftUI

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
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.borderedProminent)
        .tint(Palette.accent)
        #if !os(tvOS)
        .controlSize(.large)   // ControlSize is unavailable on tvOS
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
            Text("plurx")
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
    @State private var url = ""

    var body: some View {
        AuthScaffold(subtitle: "Connect to your server", error: model.authError) {
            TextField("192.168.1.10:32600", text: $url)
                .plurxFieldStyle()
                .font(.system(.body, design: .monospaced))
                #if os(iOS)
                .keyboardType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                #endif
                .onSubmit { connect() }

            PrimaryButton(title: "Connect", busy: model.busy, disabled: url.isEmpty) { connect() }

            Text("Enter the address shown in plurx → Settings, e.g. http://192.168.1.10:32600")
                .font(.system(.caption2, design: .monospaced))
                .foregroundColor(Palette.muted)
                .multilineTextAlignment(.center)
        }
        .onAppear { if url.isEmpty { url = model.origin } }
    }

    private func connect() { Task { await model.connect(url) } }
}

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
