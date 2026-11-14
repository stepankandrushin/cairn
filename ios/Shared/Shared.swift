// What the app and its two extensions share: the App Group's database, the account signed in, its
// token in the keychain, and the backup's settings.

import Foundation
import Security

enum Cairn {
    static let appGroup = "group.storage.cairn"

    static var container: URL {
        if let url = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) {
            return url
        }
        // Without the App Group (a build signed without it): the app's own, which its extensions do
        // not share.
        return FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    }

    static var databasePath: String {
        try? FileManager.default.createDirectory(at: container, withIntermediateDirectories: true)
        return container.appendingPathComponent("cairn.db").path
    }

    static func store() throws -> Store {
        try Store.open(path: databasePath)
    }
}

/// The server signed in to, as this device. The token is in the keychain.
struct Account: Codable, Equatable {
    var server: String
    var device: String
    var deviceName: String
    var user: String
    var allowHTTP: Bool = false

    static let key = "account"

    static func load(_ store: Store) -> Account? {
        guard let data = try? store.get(key: key) else { return nil }
        return try? JSONDecoder().decode(Account.self, from: data)
    }

    func save(_ store: Store) throws {
        try store.set(key: Account.key, value: JSONEncoder().encode(self))
    }

    func session() throws -> Session {
        guard let token = Keychain.token() else {
            throw CairnError.Refused(status: 401, reason: "no token on this phone: sign in again")
        }
        return try Session(server: server, token: token, device: device, deviceName: deviceName, allowHttp: allowHTTP)
    }
}

/// The token, readable by the app and its extensions, after the phone's first unlock.
enum Keychain {
    private static let service = "storage.cairn.token"

    private static var base: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: "token",
            kSecAttrAccessGroup as String: Cairn.appGroup,
        ]
    }

    static func token() -> String? {
        var q = base
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let data = out as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    static func save(_ token: String) throws {
        SecItemDelete(base as CFDictionary)
        var q = base
        q[kSecValueData as String] = Data(token.utf8)
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(q as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw CairnError.Failed(reason: "the keychain refused the token (\(status))")
        }
    }

    static func delete() {
        SecItemDelete(base as CFDictionary)
    }
}

/// Where photos go and what they are called, and whether backup is on.
struct Backup: Codable, Equatable {
    var enabled = false
    var folder = ""
    var path = "Photos"
    var originalNames = false
    var subfolders = false
    /// The whole library, or only what is taken from `since` on.
    var everything = false
    var since = Date()

    static let key = "backup"

    static func load(_ store: Store) -> Backup {
        guard let data = try? store.get(key: key), let b = try? JSONDecoder().decode(Backup.self, from: data) else {
            return Backup()
        }
        return b
    }

    func save(_ store: Store) throws {
        try store.set(key: Backup.key, value: JSONEncoder().encode(self))
    }

    var destination: Destination {
        Destination(folder: folder, path: path, style: originalNames ? .original : .nextcloud, subfolders: subfolders)
    }
}

/// The folders shown in the Files app, each a File Provider domain named by its folder.
enum FilesDomains {
    static let key = "files.folders"

    static func load(_ store: Store) -> [String] {
        guard let data = try? store.get(key: key), let v = try? JSONDecoder().decode([String].self, from: data) else {
            return []
        }
        return v
    }

    static func save(_ store: Store, _ folders: [String]) throws {
        try store.set(key: key, value: JSONEncoder().encode(folders))
    }
}

extension CairnError {
    /// What a person reads: the core's reason as it gave it.
    var message: String {
        switch self {
        case let .Refused(_, reason), let .NotFound(reason), let .Unreachable(reason), let .Failed(reason):
            return reason
        case .Cancelled:
            return "stopped"
        }
    }
}

/// An error in words: the core's own reason, or the system's description.
func describe(_ error: Error) -> String {
    (error as? CairnError)?.message ?? error.localizedDescription
}

/// A request from the core, as Foundation sends one.
extension HttpRequest {
    var urlRequest: URLRequest {
        var r = URLRequest(url: URL(string: url)!)
        r.httpMethod = method
        for h in headers {
            r.setValue(h.value, forHTTPHeaderField: h.name)
        }
        return r
    }
}

/// A transfer's progress, told to a `Progress` the system or the app shows, and stopped when that is
/// cancelled.
final class Watch: TransferProgress {
    let system: Foundation.Progress
    init(_ p: Foundation.Progress) { system = p }
    func progress(done: UInt64, total: UInt64) -> Bool {
        system.completedUnitCount = Int64(done)
        return !system.isCancelled
    }
}

func hex(_ d: Data) -> String {
    d.map { String(format: "%02x", $0) }.joined()
}
