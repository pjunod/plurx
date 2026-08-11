import Foundation

/// One class per way of failing to reach the server. The raw values are the
/// class ids in `tests/contracts/connectivity-copy.json`, shared with the web
/// and Android clients; see docs/CLIENT-CONNECTIVITY.md §1.
enum ConnectionFailure: String, CaseIterable {
    case offline
    case unreachable
    case unknownHost = "unknown_host"
    case timeout
    case insecure
    case serverError = "server_error"
    case unknown
}

/// What a surface offers the viewer for a given class. `retry` is on every
/// class; `change_server` only where a different address is a plausible fix.
enum ConnectionAction: String, CaseIterable {
    case retry
    case changeServer = "change_server"

    var label: String {
        switch self {
        case .retry: return "Try again"
        case .changeServer: return "Change server"
        }
    }
}

/// The four strings a class carries: a headline and a sentence for a full
/// surface, a single line for an inline notice, and the actions to offer.
struct ConnectionCopy {
    let title: String
    let detail: String
    let short: String
    let actions: [ConnectionAction]
}

/// Which of docs §5's shapes a failure gets on a given screen.
///
/// A value rather than an `if` buried in a view body, so the contract's
/// `cached_content_wins` rule is pinned by test: a refresh that fails over
/// content the viewer is already reading gets one `short` line above it, and
/// only a screen with nothing to show is replaced by the full error state.
enum ConnectionSurface: Equatable {
    case none
    case full(ConnectionFailure)
    case notice(ConnectionFailure)
}

/// The Apple client's one classifier, and the only place connectivity copy is
/// constructed.
///
/// **`tests/contracts/connectivity-copy.json` is the source of truth for every
/// string below.** They are transcribed here verbatim, with `{server}` written
/// as the interpolation it stands for, because Swift cannot read the contract
/// at build time. `AppleClientTests.testConnectionCopyIsByteIdenticalToTheSharedContract`
/// decodes that JSON out of the test bundle and fails if a single character
/// drifts, so edit the contract first and this file second — never only one.
enum Connectivity {
    /// `server_fallback` in the contract: what `{server}` becomes when neither
    /// a display name nor an origin is known.
    static let serverFallback = "the server"

    /// `credentials_message` in the contract. The one sentence a client says
    /// *instead of* a class, and only for HTTP 401/403 (docs §4). It lives
    /// beside the classes for the same reason they do: a client that words it
    /// differently has quietly reopened the split this taxonomy closed.
    static let credentialsMessage = "Wrong username or password"

    /// Pure, and deliberately total apart from two holes: cancellation is
    /// control flow rather than a failure, and 401/403 belongs to the auth
    /// path (docs §4), so both return nil and the caller keeps its own
    /// handling. Everything else lands on a class — `unknown` at worst — so a
    /// native Foundation sentence can never be what a viewer reads.
    /// The `URLError.Code` mapping is docs/CLIENT-CONNECTIVITY.md §2.3.
    nonisolated static func classify(_ error: Error) -> ConnectionFailure? {
        if error is CancellationError { return nil }

        if let urlError = error as? URLError {
            switch urlError.code {
            case .cancelled:
                return nil
            case .notConnectedToInternet, .internationalRoamingOff, .callIsActive:
                return .offline
            case .cannotFindHost, .dnsLookupFailed:
                return .unknownHost
            case .timedOut:
                return .timeout
            case .secureConnectionFailed,
                 .serverCertificateHasBadDate,
                 .serverCertificateUntrusted,
                 .serverCertificateHasUnknownRoot,
                 .serverCertificateNotYetValid,
                 .clientCertificateRejected,
                 .clientCertificateRequired,
                 .appTransportSecurityRequiresSecureConnection:
                return .insecure
            case .cannotConnectToHost,
                 .networkConnectionLost,
                 .cannotLoadFromNetwork,
                 .resourceUnavailable,
                 .badServerResponse:
                return .unreachable
            default:
                // §2.3: any other `URLError` is `unreachable`. It is the most
                // common true cause, and its copy stays honest when it is not.
                return .unreachable
            }
        }

        if let apiError = error as? APIError {
            switch apiError {
            case .connection(let failure):
                // Already classified upstream by `PlurxAPI.transportError`.
                // Re-classifying must be the identity, or every screen that
                // asks a second time would flatten the class to `unknown`.
                return failure
            case .http(let code):
                if code == 401 || code == 403 { return nil }
                return (500..<600).contains(code) ? .serverError : .unknown
            case .badURL, .transport:
                return .unknown
            }
        }

        // A body the client cannot use is the server answering wrongly, not the
        // network failing.
        if error is DecodingError { return .serverError }

        return .unknown
    }

    /// The contract copy for a class, with `{server}` resolved. Pass the
    /// server's display name, falling back to its origin; nil or blank becomes
    /// the contract's `server_fallback`.
    nonisolated static func copy(
        for failure: ConnectionFailure,
        server: String?
    ) -> ConnectionCopy {
        let name = displayName(server)
        switch failure {
        case .offline:
            return ConnectionCopy(
                title: "You're offline",
                detail: "This device isn't connected to a network.",
                short: "You're offline.",
                actions: [.retry]
            )
        case .unreachable:
            return ConnectionCopy(
                title: "Can't reach \(name)",
                detail: "The network is working, but the server didn't answer. It may be powered off, restarting, or on another network.",
                short: "Can't reach \(name).",
                actions: [.retry, .changeServer]
            )
        case .unknownHost:
            return ConnectionCopy(
                title: "Can't find \(name)",
                detail: "Nothing on this network answers to that address. If the server moved, point Cinema at its new one.",
                short: "Can't find \(name).",
                actions: [.retry, .changeServer]
            )
        case .timeout:
            return ConnectionCopy(
                title: "No answer from \(name)",
                detail: "The server accepted the connection but didn't answer in time. It may be busy or still starting up.",
                short: "No answer from \(name).",
                actions: [.retry]
            )
        case .insecure:
            return ConnectionCopy(
                title: "Couldn't connect securely to \(name)",
                detail: "The secure connection failed. The server's certificate may have changed or expired.",
                short: "Couldn't connect securely to \(name).",
                actions: [.retry, .changeServer]
            )
        case .serverError:
            return ConnectionCopy(
                title: "Error from \(name)",
                detail: "The server answered with an error. Nothing is wrong with this device or your network.",
                short: "Error from \(name).",
                actions: [.retry]
            )
        case .unknown:
            return ConnectionCopy(
                title: "Something went wrong",
                detail: "Cinema couldn't complete that request.",
                short: "Something went wrong.",
                actions: [.retry]
            )
        }
    }

    /// The short line for a failure the classifier could actually place, or nil
    /// when it could not and the caller's own copy should stand instead. Use
    /// this where a screen already has a better sentence for its own errors —
    /// playback preparation, Picture in Picture — and only the transport case
    /// should be taken over.
    nonisolated static func classifiedMessage(
        for error: Error,
        server: String?
    ) -> String? {
        guard let failure = classify(error), failure != .unknown else { return nil }
        return copy(for: failure, server: server).short
    }

    /// The single line a screen shows for a failed request: its connectivity
    /// class when it has one, otherwise Cinema's own sentence for the
    /// conditions Cinema defines that are not about reaching a server,
    /// otherwise the `unknown` class.
    ///
    /// Nil means *say nothing*: cancellation is the caller stopping, not a
    /// failure, and every other layer here is already transparent to it.
    ///
    /// The guarantee that no Foundation string can come out of here is not
    /// structural — it rests on `ownCopy` returning only strings this codebase
    /// wrote. See the note there.
    nonisolated static func message(for error: Error, server: String?) -> String? {
        if isCancellation(error) { return nil }
        return classifiedMessage(for: error, server: server)
            ?? ownCopy(for: error)
            ?? copy(for: .unknown, server: server).short
    }

    /// Which shape a failure gets on a screen (docs §5).
    nonisolated static func surface(
        for failure: ConnectionFailure?,
        hasCachedContent: Bool
    ) -> ConnectionSurface {
        guard let failure else { return .none }
        return hasCachedContent ? .notice(failure) : .full(failure)
    }

    /// Cancellation reaches this client as either Swift's `CancellationError`
    /// or `NSURLErrorCancelled`, depending on which layer noticed first.
    nonisolated static func isCancellation(_ error: Error) -> Bool {
        if error is CancellationError { return true }
        if let urlError = error as? URLError, urlError.code == .cancelled { return true }
        return false
    }

    /// Copy Cinema wrote for a condition outside the taxonomy — "Not enough
    /// device storage for this download", a Bonjour name that stopped
    /// answering, an unusable PGS manifest.
    ///
    /// The list is explicit rather than a `LocalizedError` cast on purpose: a
    /// cast would also let anything Foundation happens to describe through,
    /// which is the exact leak this taxonomy exists to close. `APIError`'s
    /// `.badURL` and `.http` are excluded because "Server returned 404" is
    /// debug shorthand, not user copy — those land on `unknown`, which is what
    /// the full-surface path says for them too. `.transport` is included
    /// because every one of its construction sites passes a literal this
    /// codebase wrote; `PlurxAPI.transportError` no longer builds one from
    /// `error.localizedDescription`, and that is the whole of the guarantee.
    private nonisolated static func ownCopy(for error: Error) -> String? {
        switch error {
        case let apiError as APIError:
            guard case .transport(let sentence) = apiError else { return nil }
            return sentence
        case let discoveryError as ServerDiscoveryError:
            return discoveryError.errorDescription
        case let overlayError as PGSOverlayError:
            return overlayError.errorDescription
        default:
            return nil
        }
    }

    private nonisolated static func displayName(_ server: String?) -> String {
        guard let server,
              !server.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return serverFallback }
        return server
    }
}
