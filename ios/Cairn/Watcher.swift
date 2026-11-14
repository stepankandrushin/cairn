// While the app is open, each folder shown in the Files app is watched on the server's change feed,
// and the Files app told as soon as something changes there.

import Foundation

final class Watcher: @unchecked Sendable {
    static let shared = Watcher()
    private var tasks: [Task<Void, Never>] = []
    private let lock = NSLock()

    func restart() {
        stop()
        guard let store = try? Cairn.store(), let account = Account.load(store) else { return }
        let folders = FilesDomains.load(store)
        lock.lock(); defer { lock.unlock() }
        for name in folders {
            tasks.append(Task.detached(priority: .background) {
                var folder: Folder?
                var head: UInt64?
                while !Task.isCancelled {
                    do {
                        if folder == nil { folder = try account.session().folder(folder: name) }
                        let now = try folder!.waitForHead(after: head ?? 0, seconds: 25)
                        if let h = head, now != h { Model.signalFiles([name]) }
                        head = now
                    } catch {
                        folder = nil
                        try? await Task.sleep(nanoseconds: 30_000_000_000)
                    }
                }
            })
        }
    }

    func stop() {
        lock.lock(); defer { lock.unlock() }
        tasks.forEach { $0.cancel() }
        tasks = []
    }
}
