// An item of the Files app: a file or folder of the server, by its object's ID.

import FileProvider
import UniformTypeIdentifiers

final class Item: NSObject, NSFileProviderItem {
    let core: FpItem

    init(_ i: FpItem) {
        core = i
    }

    /// The core's identifier for the system's: `root` is the root container.
    static func core(_ id: NSFileProviderItemIdentifier) -> String {
        id == .rootContainer ? "root" : id.rawValue
    }

    static func system(_ id: String) -> NSFileProviderItemIdentifier {
        id == "root" ? .rootContainer : NSFileProviderItemIdentifier(id)
    }

    var itemIdentifier: NSFileProviderItemIdentifier { Item.system(core.identifier) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { Item.system(core.parent) }
    var filename: String { core.filename }

    var contentType: UTType {
        if core.isDir { return .folder }
        let ext = (core.filename as NSString).pathExtension
        return UTType(filenameExtension: ext) ?? .data
    }

    var documentSize: NSNumber? { core.isDir ? nil : NSNumber(value: core.size) }

    var contentModificationDate: Date? {
        Date(timeIntervalSince1970: Double(core.mtimeSec) + Double(core.mtimeNsec) / 1e9)
    }

    var capabilities: NSFileProviderItemCapabilities {
        if core.isDir {
            var c: NSFileProviderItemCapabilities = [.allowsReading, .allowsContentEnumerating]
            if core.writable { c.insert(.allowsAddingSubItems) }
            return c
        }
        #if targetEnvironment(simulator)
        // The simulator's File Provider daemon is not root on the Mac, and cannot lock a placeholder
        // that may not be written (EPERM, `cannotSetMetadata`), so no file would show: there they are
        // shown writable, and an edit is still refused by `modifyItem`. A phone locks them.
        return [.allowsReading, .allowsWriting]
        #else
        return [.allowsReading]
        #endif
    }

    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: core.contentVersion, metadataVersion: core.metadataVersion)
    }

    /// Writable on the phone's disk whatever the server allows: the system cannot finish making a
    /// read-only placeholder (it sets its metadata after its mode). What Files may do is the
    /// capabilities' to say.
    var fileSystemFlags: NSFileProviderFileSystemFlags {
        var f: NSFileProviderFileSystemFlags = [.userReadable, .userWritable]
        if core.isDir || core.mode & 0o100 != 0 { f.insert(.userExecutable) }
        return f
    }
}
