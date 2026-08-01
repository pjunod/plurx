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
        static let token = "plurx.token"
        static let username = "plurx.username"
        static let audioLang = "plurx.audioLang"
        static let subLang = "plurx.subLang"
        static let autoplay = "plurx.autoplay"
        static let libraryGrouping = "plurx.libraryGrouping"
        static let posterSize = "plurx.posterSize"
    }

    var origin: String {
        get { defaults.string(forKey: Key.origin) ?? "" }
        nonmutating set { defaults.set(newValue, forKey: Key.origin) }
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
}
