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
        static let autoplay = "plurx.autoplay"
        static let libraryGrouping = "plurx.libraryGrouping"
        static let posterSize = "plurx.posterSize"
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
    func clearToken() { defaults.removeObject(forKey: Key.token) }
}
