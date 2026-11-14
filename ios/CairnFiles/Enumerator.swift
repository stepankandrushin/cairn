// What the system is told is in a folder, and, for the working set, what changed since it last asked:
// from the server's change feed, its anchor the journal's sequence.

import FileProvider
import Foundation

final class Enumerator: NSObject, NSFileProviderEnumerator {
    private let ext: FileProviderExtension
    private let container: NSFileProviderItemIdentifier

    init(_ ext: FileProviderExtension, _ container: NSFileProviderItemIdentifier) {
        self.ext = ext
        self.container = container
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        DispatchQueue.global(qos: .userInitiated).async {
            if self.container == .workingSet || self.container == .trashContainer {
                // The working set is what the system has seen of the folders it opened; its changes
                // come by `enumerateChanges`.
                observer.finishEnumerating(upTo: nil)
                return
            }
            do {
                let page = try self.ext.folder().fpList(dir: Item.core(self.container))
                Skipped.note(page.skipped, folder: self.ext.domain.identifier.rawValue)
                observer.didEnumerate(page.items.map(Item.init))
                observer.finishEnumerating(upTo: nil)
            } catch {
                observer.finishEnumeratingWithError(FileProviderExtension.mapped(error))
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async {
            completionHandler((try? self.ext.folder().head()).map(Enumerator.anchor))
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        DispatchQueue.global(qos: .userInitiated).async {
            guard self.container == .workingSet else {
                // A folder's changes come in the working set's.
                observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
                return
            }
            guard let after = Enumerator.seq(anchor) else {
                observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
                return
            }
            do {
                let c = try self.ext.folder().fpChanges(after: after, limit: 200)
                Skipped.note(c.skipped, folder: self.ext.domain.identifier.rawValue)
                if !c.updated.isEmpty { observer.didUpdate(c.updated.map(Item.init)) }
                if !c.deleted.isEmpty { observer.didDeleteItems(withIdentifiers: c.deleted.map(Item.system)) }
                observer.finishEnumeratingChanges(upTo: Enumerator.anchor(c.next), moreComing: c.more)
            } catch {
                observer.finishEnumeratingWithError(FileProviderExtension.mapped(error))
            }
        }
    }

    static func anchor(_ seq: UInt64) -> NSFileProviderSyncAnchor {
        NSFileProviderSyncAnchor(withUnsafeBytes(of: seq.bigEndian) { Data($0) })
    }

    static func seq(_ a: NSFileProviderSyncAnchor) -> UInt64? {
        guard a.rawValue.count == 8 else { return nil }
        return a.rawValue.reduce(0) { $0 << 8 | UInt64($1) }
    }
}

/// What the Files app was not shown, kept for the app to list with the reason (invariant 8).
enum Skipped {
    static func note(_ s: [FpSkipped], folder: String) {
        guard !s.isEmpty, let store = try? Cairn.store() else { return }
        try? store.noteSkipped(entries: s.map {
            SkippedEntry(folder: folder, object: $0.object, display: $0.display, reason: $0.reason)
        })
    }
}
