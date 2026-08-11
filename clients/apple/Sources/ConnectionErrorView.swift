import SwiftUI

/// The full-surface rendering of one connectivity class
/// (docs/CLIENT-CONNECTIVITY.md §5): the class's `title`, its `detail`, and a
/// button for every action the contract lists for it.
///
/// The five screens that used to hand-instantiate `ContentUnavailableView` with
/// a raw error string share this instead, so "the server is gone" looks and
/// reads the same everywhere — and, because `retry` is on every class, is
/// always something a viewer can act on rather than a dead end.
struct ConnectionErrorView: View {
    let failure: ConnectionFailure
    /// The server's display name, falling back to its origin. Nil renders the
    /// contract's `server_fallback`.
    var server: String?
    /// An action is only drawn when its handler is supplied: `change_server`
    /// on a surface with nowhere to send the viewer would be a dead button,
    /// which is worse than one action.
    var retry: (() -> Void)?
    var changeServer: (() -> Void)?

    private var copy: ConnectionCopy {
        Connectivity.copy(for: failure, server: server)
    }

    var body: some View {
        VStack(spacing: 20) {
            ContentUnavailableView(
                copy.title,
                systemImage: Self.symbol(for: failure),
                description: Text(copy.detail)
            )
            actions
        }
        .frame(maxWidth: .infinity)
    }

    private var actions: some View {
        HStack(spacing: 12) {
            ForEach(
                Self.renderedActions(
                    for: failure,
                    canRetry: retry != nil,
                    canChangeServer: changeServer != nil
                ),
                id: \.self
            ) { action in
                button(action)
            }
        }
    }

    /// The actions this surface will actually draw: the class's own actions,
    /// minus any the caller gave no handler for.
    ///
    /// Pure and `static` so the contract's `every_error_offers_retry` can be
    /// asserted directly. A view body is not reachable from a unit test, so a
    /// rule that lived only inside one would be a rule nothing checks.
    nonisolated static func renderedActions(
        for failure: ConnectionFailure,
        canRetry: Bool,
        canChangeServer: Bool
    ) -> [ConnectionAction] {
        Connectivity.copy(for: failure, server: nil).actions.filter { action in
            switch action {
            case .retry: return canRetry
            case .changeServer: return canChangeServer
            }
        }
    }

    private func perform(_ action: ConnectionAction) {
        switch action {
        case .retry: retry?()
        case .changeServer: changeServer?()
        }
    }

    /// tvOS gets the same readable style the auth and player actions use;
    /// `TVReadableButtonStyle` owns both halves of the contrast pair, which the
    /// system bordered style does not on tvOS 26.
    private func button(_ action: ConnectionAction) -> some View {
        #if os(tvOS)
        return Button(action.label) { perform(action) }
            .buttonStyle(TVReadableButtonStyle(prominent: action == .retry))
        #else
        return Button(action.label) { perform(action) }
            .buttonStyle(.bordered)
            .tint(Palette.accent)
        #endif
    }

    /// Deliberately plain, long-lived SF Symbols: the icon is decoration for
    /// copy that already says what happened.
    static func symbol(for failure: ConnectionFailure) -> String {
        switch failure {
        case .offline: return "wifi.slash"
        case .unknownHost: return "questionmark.circle"
        case .timeout: return "clock"
        case .insecure: return "lock.slash"
        case .unreachable, .serverError, .unknown: return "exclamationmark.triangle"
        }
    }
}

/// The transient-notice shape (docs/CLIENT-CONNECTIVITY.md §5,
/// `cached_content_wins`): a refresh failed over content the viewer is already
/// reading, so the class gets one `short` line above it and a Retry, and the
/// content itself is left alone.
///
/// Without this the Apple client had only the full state, so a failed refresh
/// over a populated dashboard said nothing at all — the viewer saw stale
/// shelves and no reason for them.
struct ConnectionNoticeView: View {
    let failure: ConnectionFailure
    var server: String?
    let retry: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Text(Connectivity.copy(for: failure, server: server).short)
                .font(.system(.footnote, design: .monospaced))
                .foregroundColor(Palette.muted)
                .multilineTextAlignment(.leading)
                .frame(maxWidth: .infinity, alignment: .leading)
            // `every_error_offers_retry`: a banner is a surface too.
            Button(ConnectionAction.retry.label, action: retry)
                .font(.system(.footnote, design: .monospaced))
                .buttonStyle(.plain)
                .foregroundColor(Palette.accent)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(Palette.surface, in: RoundedRectangle(cornerRadius: 10))
    }
}
