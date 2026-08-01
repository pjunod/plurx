import Combine
import Darwin
import Foundation
import Network

enum PlurxClientDefaults {
    static let port = 32400
    static let bonjourServiceType = "_plurx._tcp"
}

/// One service returned by Bonjour. Browsing intentionally stops at the
/// service name; Apple recommends resolving only the server a person chooses,
/// rather than probing every host that announces itself.
struct DiscoveredServer: Identifiable {
    let id: String
    let name: String
    fileprivate let type: String
    fileprivate let domain: String

    fileprivate init?(endpoint: NWEndpoint) {
        guard case .service(let name, let type, let domain, _) = endpoint else { return nil }
        self.id = "\(name).\(type).\(domain)"
        self.name = name
        self.type = type
        self.domain = domain
    }
}

enum ServerDiscoveryError: Error, LocalizedError {
    case invalidService
    case resolutionFailed

    var errorDescription: String? {
        switch self {
        case .invalidService:
            return "The discovered server did not publish a usable address."
        case .resolutionFailed:
            return "The discovered server stopped responding. Try scanning again or add it manually."
        }
    }
}

/// Browses the LAN for plurx's `_plurx._tcp` DNS-SD service. The browser is
/// the default setup path; manual host entry remains available when multicast
/// is blocked by a guest network, VLAN, VPN, or container bridge.
@MainActor
final class ServerDiscovery: ObservableObject {
    @Published private(set) var servers: [DiscoveredServer] = []
    @Published private(set) var isSearching = false
    @Published private(set) var errorMessage: String?

    private var browser: NWBrowser?
    private var resolvers: [String: BonjourResolver] = [:]

    func start() {
        guard browser == nil else { return }
        errorMessage = nil
        isSearching = true

        let parameters = NWParameters.tcp
        parameters.includePeerToPeer = true
        let browser = NWBrowser(
            for: .bonjour(type: PlurxClientDefaults.bonjourServiceType, domain: nil),
            using: parameters
        )
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            Task { @MainActor in self?.update(results) }
        }
        browser.stateUpdateHandler = { [weak self] state in
            Task { @MainActor in self?.update(state) }
        }
        self.browser = browser
        browser.start(queue: .main)
    }

    func restart() {
        stop()
        servers = []
        start()
    }

    func stop() {
        browser?.cancel()
        browser = nil
        isSearching = false
    }

    func resolve(_ server: DiscoveredServer) async throws -> String {
        let resolver = BonjourResolver(server: server)
        resolvers[server.id] = resolver
        defer { resolvers.removeValue(forKey: server.id) }
        return try await resolver.resolve()
    }

    /// Give Bonjour a short window to populate after app launch or a network
    /// handoff. Saved-session recovery uses these candidates to find the same
    /// stable server instance at its current address.
    func availableServers(timeoutMs: UInt64 = 3_000) async -> [DiscoveredServer] {
        start()
        let intervalMs: UInt64 = 150
        var waitedMs: UInt64 = 0
        while servers.isEmpty && waitedMs < timeoutMs && !Task.isCancelled {
            try? await Task.sleep(nanoseconds: intervalMs * 1_000_000)
            waitedMs += intervalMs
        }
        return servers
    }

    private func update(_ results: Set<NWBrowser.Result>) {
        servers = results
            .compactMap { DiscoveredServer(endpoint: $0.endpoint) }
            .sorted {
                let byName = $0.name.localizedCaseInsensitiveCompare($1.name)
                return byName == .orderedSame ? $0.id < $1.id : byName == .orderedAscending
            }
    }

    private func update(_ state: NWBrowser.State) {
        switch state {
        case .setup, .ready:
            errorMessage = nil
        case .waiting:
            errorMessage = "Discovery is waiting for local-network access. You can still add a server manually."
        case .failed(let error):
            isSearching = false
            errorMessage = "Local-network discovery failed: \(error.localizedDescription)"
        case .cancelled:
            isSearching = false
        @unknown default:
            break
        }
    }
}

/// `NWBrowser` deliberately returns a Bonjour service rather than resolving
/// every result. Foundation's resolver supplies fresh addresses only after a
/// person chooses one server. Prefer that numeric address so reconnecting also
/// escapes a stale `.local` DNS cache.
private final class BonjourResolver: NSObject, NetServiceDelegate {
    private let service: NetService
    private var continuation: CheckedContinuation<String, Error>?
    private var finished = false

    init(server: DiscoveredServer) {
        let type = server.type.hasSuffix(".") ? server.type : server.type + "."
        let domain = server.domain.hasSuffix(".") ? server.domain : server.domain + "."
        service = NetService(domain: domain, type: type, name: server.name)
        super.init()
    }

    func resolve() async throws -> String {
        try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
            service.delegate = self
            service.resolve(withTimeout: 5)
        }
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        guard sender.port > 0,
              let host = BonjourAddress.numericHost(from: sender.addresses ?? [])
                ?? sender.hostName else {
            finish(.failure(ServerDiscoveryError.invalidService))
            return
        }
        finish(.success(BonjourAddress.origin(host: host, port: sender.port)))
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        finish(.failure(ServerDiscoveryError.resolutionFailed))
    }

    private func finish(_ result: Result<String, Error>) {
        guard !finished else { return }
        finished = true
        service.delegate = nil
        service.stop()
        continuation?.resume(with: result)
        continuation = nil
    }

}

/// Pure URL formatting kept outside the Foundation delegate so both IPv4/DNS
/// and IPv6 service resolutions can be covered without a live Bonjour lookup.
enum BonjourAddress {
    static func origin(host: String, port: Int) -> String {
        let host = host.trimmingCharacters(in: CharacterSet(charactersIn: "."))
        let escapedHost = host.replacingOccurrences(of: "%", with: "%25")
        let formattedHost = escapedHost.contains(":") && !escapedHost.hasPrefix("[")
            ? "[\(escapedHost)]"
            : escapedHost
        return "http://\(formattedHost):\(port)"
    }

    static func numericHost(from addresses: [Data]) -> String? {
        let ordered = addresses.sorted { lhs, rhs in
            addressFamily(lhs) == AF_INET && addressFamily(rhs) != AF_INET
        }
        for address in ordered {
            let host = address.withUnsafeBytes { bytes -> String? in
                guard address.count >= MemoryLayout<sockaddr>.size,
                      let base = bytes.baseAddress?.assumingMemoryBound(to: sockaddr.self) else {
                    return nil
                }
                var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
                let result = getnameinfo(
                    base,
                    socklen_t(address.count),
                    &buffer,
                    socklen_t(buffer.count),
                    nil,
                    0,
                    NI_NUMERICHOST
                )
                guard result == 0 else { return nil }
                return String(cString: buffer)
            }
            if let host, !host.isEmpty { return host }
        }
        return nil
    }

    private static func addressFamily(_ address: Data) -> Int32? {
        address.withUnsafeBytes { bytes in
            guard address.count >= MemoryLayout<sockaddr>.size,
                  let base = bytes.baseAddress?.assumingMemoryBound(to: sockaddr.self) else {
                return nil
            }
            return Int32(base.pointee.sa_family)
        }
    }
}
