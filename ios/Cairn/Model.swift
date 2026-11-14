// The app's state: the account signed in, its folders, the backup's settings and how far it is.

import FileProvider
import Foundation
import Photos
import SwiftUI

@MainActor
final class Model: ObservableObject {
    static let shared = Model()

    @Published var account: Account?
    @Published var folders: [FolderInfo] = []
    @Published var backup = Backup()
    @Published var stats = UploadStats(planned: 0, sent: 0, done: 0, failed: 0, skipped: 0)
    @Published var failed: [Upload] = []
    @Published var recent: [Upload] = []
    @Published var filesFolders: [String] = []
    @Published var skipped: [SkippedEntry] = []
    @Published var message: String?
    @Published var uploading = false
    @Published var uploadNote: String?

    let store: Store?

    private init() {
        store = try? Cairn.store()
        if let store {
            account = Account.load(store)
            backup = Backup.load(store)
            filesFolders = FilesDomains.load(store)
        }
        refresh()
    }

    var session: Session? { try? account?.session() }

    /// Signs in, checking the token with the server first; makes this device's ID the first time.
    func signIn(server: String, token: String, deviceName: String) async throws {
        guard let store else { throw CairnError.Failed(reason: "the app's database could not be opened") }
        let server = server.trimmingCharacters(in: .whitespacesAndNewlines)
        let device = (try? store.getText(key: "device")) ?? nil ?? newDeviceId()
        let allowHTTP = server.hasPrefix("http://")
        let me = try await Task.detached {
            try Session(server: server, token: token, device: device, deviceName: deviceName, allowHttp: allowHTTP).me()
        }.value
        try store.setText(key: "device", value: device)
        try Keychain.save(token.trimmingCharacters(in: .whitespacesAndNewlines))
        let a = Account(server: server, device: device, deviceName: deviceName, user: me.user, allowHTTP: allowHTTP)
        try a.save(store)
        account = a
        folders = me.folders
        if backup.folder.isEmpty, let first = me.folders.first(where: { $0.access != "read" }) {
            backup.folder = first.folder
            backup.path = "Photos/\(deviceName)"
            try backup.save(store)
        }
        if filesFolders.isEmpty, let first = me.folders.first {
            try await showInFiles(first.folder, true)
        }
    }

    func signOut() async {
        guard let store else { return }
        await stopBackup()
        for f in filesFolders { try? await showInFiles(f, false) }
        try? store.forgetAll()
        Keychain.delete()
        account = nil
        folders = []
        backup = Backup()
        refresh()
    }

    func loadFolders() async {
        guard let s = session else { return }
        do {
            folders = try await Task.detached { try s.me().folders }.value
        } catch {
            message = describe(error)
        }
    }

    func title(_ folder: String) -> String {
        folders.first(where: { $0.folder == folder })?.name ?? folder
    }

    /// Where the counts and lists stand now.
    func refresh() {
        guard let store else { return }
        stats = (try? store.stats()) ?? stats
        failed = (try? store.failed(limit: 200)) ?? []
        recent = (try? store.recent(limit: 30)) ?? []
        skipped = (try? store.skipped()) ?? []
    }

    // MARK: Backup

    func startBackup(_ b: Backup) async throws {
        guard let store else { return }
        let status = await PHPhotoLibrary.requestAuthorization(for: .readWrite)
        guard status == .authorized else {
            throw CairnError.Failed(reason: "Cairn needs access to all photos to back them up: allow it in Settings, Privacy, Photos")
        }
        var b = b
        b.enabled = true
        b.since = Date()
        try PhotoLibrary.restart(store)
        try b.save(store)
        backup = b
        Background.enablePhotosExtension(true)
        Background.schedule()
        Uploader.shared.start(reason: "backup turned on")
    }

    func stopBackup() async {
        guard let store else { return }
        backup.enabled = false
        try? backup.save(store)
        Background.enablePhotosExtension(false)
        Uploader.shared.stop()
    }

    func retry(_ u: Upload) {
        _ = try? store?.retry(id: u.id)
        refresh()
        Uploader.shared.start(reason: "retry")
    }

    func retryAll() {
        for u in failed { _ = try? store?.retry(id: u.id) }
        refresh()
        Uploader.shared.start(reason: "retry")
    }

    // MARK: Files app

    func showInFiles(_ folder: String, _ on: Bool) async throws {
        guard let store else { return }
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(folder), displayName: "Cairn: \(title(folder))")
        if on {
            try await NSFileProviderManager.add(domain)
            if !filesFolders.contains(folder) { filesFolders.append(folder) }
        } else {
            try? await NSFileProviderManager.remove(domain)
            filesFolders.removeAll { $0 == folder }
        }
        try FilesDomains.save(store, filesFolders)
        Watcher.shared.restart()
    }

    /// Tells the Files app to ask what changed, in each folder it shows.
    nonisolated static func signalFiles(_ folders: [String]) {
        for f in folders {
            let d = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(f), displayName: f)
            NSFileProviderManager(for: d)?.signalEnumerator(for: .workingSet) { _ in }
        }
    }
}
