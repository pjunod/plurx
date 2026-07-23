import Foundation

/// Persisted server + token (for silent reconnect) and the default audio /
/// subtitle languages, in UserDefaults. English out of the box.
struct SettingsStore {
    private let defaults = UserDefaults.standard

    private enum Key {
        static let origin = "plurx.origin"
        static let token = "plurx.token"
        static let username = "plurx.username"
        static let audioLang = "plurx.audioLang"
        static let subLang = "plurx.subLang"
    }

    var origin: String {
        get { defaults.string(forKey: Key.origin) ?? "" }
        nonmutating set { defaults.set(newValue, forKey: Key.origin) }
    }
    var token: String? {
        get { defaults.string(forKey: Key.token) }
        nonmutating set { defaults.set(newValue, forKey: Key.token) }
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

    /// Drop the token (sign out) but keep the origin so login stays pre-filled.
    func clearToken() { defaults.removeObject(forKey: Key.token) }
}
