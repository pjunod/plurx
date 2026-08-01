import Foundation
import Security

/// Minimal abstraction so authentication persistence can be tested without
/// putting test credentials in the process keychain.
protocol TokenStoring {
    func read() -> String?
    @discardableResult func write(_ token: String) -> Bool
    func clear()
}

/// Stores the bearer token in the Apple Keychain. Unlike UserDefaults, a
/// keychain item survives development-app replacement and is not bundled with
/// ordinary preferences, so installing a new build does not sign the viewer
/// out again.
struct TokenVault: TokenStoring {
    let service: String
    let account: String

    init(service: String = "tv.plurx.app.session", account: String = "bearer-token") {
        self.service = service
        self.account = account
    }

    func read() -> String? {
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    @discardableResult
    func write(_ token: String) -> Bool {
        guard let data = token.data(using: .utf8) else { return false }
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]

        let updated = SecItemUpdate(baseQuery as CFDictionary, attributes as CFDictionary)
        if updated == errSecSuccess { return true }
        guard updated == errSecItemNotFound else { return false }

        var newItem = baseQuery
        attributes.forEach { newItem[$0.key] = $0.value }
        return SecItemAdd(newItem as CFDictionary, nil) == errSecSuccess
    }

    func clear() {
        SecItemDelete(baseQuery as CFDictionary)
    }

    private var baseQuery: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }
}
