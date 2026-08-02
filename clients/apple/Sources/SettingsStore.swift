import Foundation

/// Persisted server, secure session token, and playback preferences. Ordinary
/// preferences live in UserDefaults; the bearer token lives in Keychain so a
/// development app replacement cannot silently sign the viewer out.
struct SettingsStore {
    private let defaults: UserDefaults
    private let tokenVault: any TokenStoring

    init(
        defaults: UserDefaults = .standard,
        tokenVault: any TokenStoring = TokenVault()
    ) {
        self.defaults = defaults
        self.tokenVault = tokenVault
    }

    private enum Key {
        static let origin = "plurx.origin"
        static let instanceId = "plurx.instanceId"
        static let token = "plurx.token"
        static let username = "plurx.username"
        static let audioLang = "plurx.audioLang"
        static let subLang = "plurx.subLang"
        static let autoplay = "plurx.autoplay"
        static let subtitleReadiness = "plurx.subtitleReadiness"
        static let libraryGrouping = "plurx.libraryGrouping"
        static let posterSize = "plurx.posterSize"
    }

    /// Read-only on purpose. Every write goes through `setServer`, which is
    /// what keeps the origin and the bearer token from drifting apart.
    var origin: String { defaults.string(forKey: Key.origin) ?? "" }
    var instanceId: String? {
        get { defaults.string(forKey: Key.instanceId) }
        nonmutating set { defaults.set(newValue, forKey: Key.instanceId) }
    }
    var token: String? {
        get {
            if let secured = tokenVault.read() { return secured }

            // One-time migration for existing installs. Keep the legacy copy
            // only if Keychain is unavailable, so a working session is never
            // discarded while improving its storage.
            guard let legacy = defaults.string(forKey: Key.token) else { return nil }
            if tokenVault.write(legacy) {
                defaults.removeObject(forKey: Key.token)
            }
            return legacy
        }
        nonmutating set {
            guard let newValue else {
                clearToken()
                return
            }
            if tokenVault.write(newValue) {
                defaults.removeObject(forKey: Key.token)
            } else {
                defaults.set(newValue, forKey: Key.token)
            }
        }
    }
    var username: String? {
        get { defaults.string(forKey: Key.username) }
        nonmutating set { defaults.set(newValue, forKey: Key.username) }
    }
    var audioLang: String {
        get { defaults.string(forKey: Key.audioLang) ?? "eng" }
        nonmutating set { defaults.set(newValue, forKey: Key.audioLang) }
    }
    var subLang: String {
        get { defaults.string(forKey: Key.subLang) ?? "eng" }
        nonmutating set { defaults.set(newValue, forKey: Key.subLang) }
    }
    /// Match the web client: autoplay next is on until explicitly disabled.
    var autoplay: Bool {
        get { defaults.object(forKey: Key.autoplay) as? Bool ?? true }
        nonmutating set { defaults.set(newValue, forKey: Key.autoplay) }
    }
    /// Defaults to `.instant`, which is what the app has always done: an
    /// unchanged install behaves exactly as before.
    var subtitleReadiness: SubtitleReadiness {
        get {
            SubtitleReadiness(rawValue: defaults.string(forKey: Key.subtitleReadiness) ?? "")
                ?? .instant
        }
        nonmutating set { defaults.set(newValue.rawValue, forKey: Key.subtitleReadiness) }
    }
    var libraryGrouping: LibraryGrouping {
        get {
            LibraryGrouping(rawValue: defaults.string(forKey: Key.libraryGrouping) ?? "")
                ?? .category
        }
        nonmutating set { defaults.set(newValue.rawValue, forKey: Key.libraryGrouping) }
    }
    var posterSize: PosterSize {
        get { PosterSize(rawValue: defaults.string(forKey: Key.posterSize) ?? "") ?? .medium }
        nonmutating set { defaults.set(newValue.rawValue, forKey: Key.posterSize) }
    }

    /// Drop the token (sign out) but keep the origin so login stays pre-filled.
    func clearToken() {
        tokenVault.clear()
        defaults.removeObject(forKey: Key.token)
    }

    /// The single owner of the "origin and token are written together or not at
    /// all" invariant. A bearer is issued by one server and is meaningless — and
    /// dangerous — at another: without this, a kill-and-relaunch between
    /// `connect(B)` and signing in sent server A's bearer to server B in
    /// cleartext, and A's still-valid session was destroyed the moment B
    /// answered 401.
    ///
    /// Callers must name the token that belongs with the address, so no path can
    /// write one without deciding the other. Pass `token: nil` for a new or
    /// changed server; pass the existing token only when the *same* server
    /// instance was rediscovered at a new address, which is a move, not a
    /// change of identity.
    func setServer(origin: String, instanceId: String?, token: String?) {
        defaults.set(origin, forKey: Key.origin)
        if let instanceId {
            defaults.set(instanceId, forKey: Key.instanceId)
        } else {
            defaults.removeObject(forKey: Key.instanceId)
        }
        guard let token else {
            clearToken()
            return
        }
        self.token = token
    }

    /// Leaving a server entirely (the "Change server" action). Address,
    /// identity, and bearer go together.
    func clearServer() {
        setServer(origin: "", instanceId: nil, token: nil)
    }
}
