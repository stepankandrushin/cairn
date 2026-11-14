// The Files app's view of a Cairn folder: each folder listed when it is opened, each file downloaded
// when it is opened and kept only as long as the system keeps it, and new files and folders sent. This
// version changes nothing that is on the server: renaming, moving, deleting and editing in place are
// not offered.

import FileProvider
import Foundation
import UniformTypeIdentifiers

final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    let domain: NSFileProviderDomain
    private var cached: Folder?
    private let lock = NSLock()

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
    }

    func invalidate() {}

    /// The folder this domain shows, asked of the server once.
    func folder() throws -> Folder {
        lock.lock(); defer { lock.unlock() }
        if let f = cached { return f }
        let store = try Cairn.store()
        guard let account = Account.load(store) else {
            throw NSFileProviderError(.notAuthenticated)
        }
        let f = try account.session().folder(folder: domain.identifier.rawValue)
        cached = f
        return f
    }

    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                guard let i = try self.folder().fpItem(identifier: Item.core(identifier)) else {
                    throw NSFileProviderError(.noSuchItem)
                }
                completionHandler(Item(i), nil)
            } catch {
                completionHandler(nil, FileProviderExtension.mapped(error))
            }
            progress.completedUnitCount = 1
        }
        return progress
    }

    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let folder = try self.folder()
                guard let i = try folder.fpItem(identifier: Item.core(itemIdentifier)), !i.isDir else {
                    throw NSFileProviderError(.noSuchItem)
                }
                let dir = try NSFileProviderManager(for: self.domain)?.temporaryDirectoryURL()
                    ?? FileManager.default.temporaryDirectory
                let dest = dir.appendingPathComponent(UUID().uuidString)
                progress.totalUnitCount = Int64(max(i.size, 1))
                let watch = Watch(progress)
                _ = try folder.download(object: i.identifier, revision: hex(i.contentVersion),
                                        dest: dest.path, size: i.size, progress: watch)
                completionHandler(dest, Item(i), nil)
            } catch {
                completionHandler(nil, nil, FileProviderExtension.mapped(error))
            }
        }
        return progress
    }

    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
                    options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let folder = try self.folder()
                let parent = Item.core(itemTemplate.parentItemIdentifier)
                let name = itemTemplate.filename
                if itemTemplate.contentType == .folder {
                    let made = try folder.mkdir(parent: parent, name: name)
                    guard let i = try folder.fpItem(identifier: made.object) else {
                        throw NSFileProviderError(.noSuchItem)
                    }
                    completionHandler(Item(i), [], false, nil)
                    return
                }
                if itemTemplate.contentType == .symbolicLink || itemTemplate.contentType?.conforms(to: .aliasFile) == true {
                    throw NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError,
                                  userInfo: [NSLocalizedDescriptionKey: "Cairn's Files folder takes files and folders, not links"])
                }
                guard let url else { throw NSFileProviderError(.noSuchItem) }
                let mtime = itemTemplate.contentModificationDate??.timeIntervalSince1970 ?? Date().timeIntervalSince1970
                let sec = Int64(mtime.rounded(.down))
                let nsec = UInt32(((mtime - Double(sec)) * 1e9).rounded().clamped(to: 0...999_999_999))
                let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize).flatMap { $0 } ?? 0
                progress.totalUnitCount = Int64(max(size, 1))
                let sent = try folder.putFile(parent: parent, name: name, local: url.path, mtimeSec: sec,
                                              mtimeNsec: nsec, rename: false, progress: Watch(progress))
                guard let i = try folder.fpItem(identifier: sent.object) else {
                    throw NSFileProviderError(.noSuchItem)
                }
                completionHandler(Item(i), [], false, nil)
            } catch CairnError.Refused(409, _) {
                completionHandler(nil, [], false, NSFileProviderError(.filenameCollision))
            } catch {
                completionHandler(nil, [], false, FileProviderExtension.mapped(error))
            }
        }
        return progress
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion,
                    changedFields: NSFileProviderItemFields, contents newContents: URL?,
                    options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        // Not offered: the item's capabilities leave it out. The server is left as it is.
        completionHandler(nil, [], false, FileProviderExtension.unsupported)
        return Progress(totalUnitCount: 0)
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        completionHandler(FileProviderExtension.unsupported)
        return Progress(totalUnitCount: 0)
    }

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        Enumerator(self, containerItemIdentifier)
    }

    static let unsupported = NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError, userInfo: [
        NSLocalizedDescriptionKey: "Cairn's Files folder does not change what is on the server yet: use the Cairn app or a computer",
    ])

    static func mapped(_ error: Error) -> Error {
        switch error {
        case CairnError.Unreachable:
            return NSFileProviderError(.serverUnreachable)
        case CairnError.NotFound:
            return NSFileProviderError(.noSuchItem)
        case CairnError.Refused(401, _):
            return NSFileProviderError(.notAuthenticated)
        case CairnError.Cancelled:
            return NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError)
        default:
            return error
        }
    }
}
