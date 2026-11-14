// What of the photo library is backed up: each asset's originals (its photo or video, a Live Photo's
// video, a RAW beside its JPEG), found by the library's persistent changes since the last look, and
// queued in the core's database, once each, before anything is sent.

import Foundation
import Photos

enum PhotoLibrary {
    /// The resource types sent: the originals. An edit's rendering, and the data to redo it, stay.
    static let sent: Set<PHAssetResourceType> = [.photo, .video, .audio, .alternatePhoto, .pairedVideo]

    private static let tokenKey = "photos.token"
    private static let cursorKey = "photos.scanned-to"

    static var authorized: Bool {
        PHPhotoLibrary.authorizationStatus(for: .readWrite) == .authorized
    }

    static func filename(_ r: PHAssetResource) -> String {
        if #available(iOS 27.0, *), let f = r.filename {
            return f
        }
        return r.originalFilename
    }

    /// The resources of an asset that are sent, as the core queues them.
    static func resources(of asset: PHAsset) -> [(PHAssetResource, AssetResource)] {
        let created = asset.creationDate ?? asset.modificationDate ?? Date()
        let secs = created.timeIntervalSince1970
        let sec = Int64(secs.rounded(.down))
        let nsec = UInt32(((secs - Double(sec)) * 1_000_000_000).rounded().clamped(to: 0...999_999_999))
        let offset = Int32(TimeZone.current.secondsFromGMT(for: created))
        var seen: [PHAssetResourceType: UInt32] = [:]
        var out: [(PHAssetResource, AssetResource)] = []
        for r in PHAssetResource.assetResources(for: asset) where sent.contains(r.type) {
            let idx = seen[r.type, default: 0]
            seen[r.type] = idx + 1
            out.append((r, AssetResource(
                asset: asset.localIdentifier, kind: Int32(r.type.rawValue), idx: idx,
                originalName: filename(r), createdSec: sec, createdNsec: nsec, utcOffset: offset)))
        }
        return out
    }

    /// The resource an upload sends, if the asset is still in the library.
    static func resource(for u: Upload) -> PHAssetResource? {
        let found = PHAsset.fetchAssets(withLocalIdentifiers: [u.asset], options: nil)
        guard let asset = found.firstObject else { return nil }
        return PHAssetResource.assetResources(for: asset)
            .filter { $0.type.rawValue == Int(u.kind) }
            .dropFirst(Int(u.idx)).first
    }

    /// Queues what is new since the last look, at most about `limit` assets: answers whether more is
    /// left. The first look after backup is turned on starts the library's change history and, for
    /// the whole library, goes through it oldest first.
    @discardableResult
    static func queueNew(_ store: Store, limit: Int = 500) throws -> Bool {
        let backup = Backup.load(store)
        guard backup.enabled, authorized else { return false }
        let lib = PHPhotoLibrary.shared()
        var more = false

        // What was there before: everything, or what is taken from `since` on, oldest first, `limit` at
        // a time. The cursor is the creation time looked at up to, or `done`.
        let cursor = (try? store.getText(key: cursorKey)) ?? nil
        if cursor != "done" {
            if ((try? store.get(key: tokenKey)) ?? nil) == nil {
                // The history starts now, so that nothing added while this goes through is missed.
                try saveToken(store, lib.currentChangeToken)
            }
            var from = cursor.flatMap(Double.init)
            if !backup.everything {
                from = max(from ?? -Double.greatestFiniteMagnitude, backup.since.timeIntervalSince1970)
            }
            let opts = PHFetchOptions()
            opts.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: true)]
            if let from {
                opts.predicate = NSPredicate(format: "creationDate >= %@", Date(timeIntervalSince1970: from) as NSDate)
            }
            opts.fetchLimit = limit
            let assets = PHAsset.fetchAssets(with: opts)
            var last = from ?? -Double.greatestFiniteMagnitude
            var batch: [AssetResource] = []
            assets.enumerateObjects { a, _, _ in
                batch.append(contentsOf: resources(of: a).map(\.1))
                if let d = a.creationDate { last = max(last, d.timeIntervalSince1970) }
            }
            if !batch.isEmpty {
                // Queued again, an asset is answered as it stands: nothing twice.
                _ = try store.plan(dest: backup.destination, resources: batch)
            }
            if assets.count >= limit {
                // More than `limit` of one time would come back for ever: past it.
                if let from, last <= from { last = from + 0.001 }
                try store.setText(key: cursorKey, value: String(last))
                return true
            }
            try store.setText(key: cursorKey, value: "done")
        }

        // Then what the library says was added since.
        guard let token = loadToken(store) else {
            try saveToken(store, lib.currentChangeToken)
            return false
        }
        do {
            let changes = try lib.fetchPersistentChanges(since: token)
            var ids = Set<String>()
            var lastToken: PHPersistentChangeToken?
            for change in changes {
                if let d = try? change.changeDetails(for: .asset) {
                    ids.formUnion(d.insertedLocalIdentifiers)
                }
                lastToken = change.changeToken
                if ids.count >= limit { more = true; break }
            }
            if !ids.isEmpty {
                let assets = PHAsset.fetchAssets(withLocalIdentifiers: Array(ids), options: nil)
                var batch: [AssetResource] = []
                assets.enumerateObjects { a, _, _ in
                    batch.append(contentsOf: resources(of: a).map(\.1))
                }
                if !batch.isEmpty {
                    _ = try store.plan(dest: backup.destination, resources: batch)
                }
            }
            if let t = lastToken {
                try saveToken(store, t)
            }
        } catch let e as PHPhotosError where e.code == .persistentChangeTokenExpired {
            // The history no longer reaches back so far: everything since backup began is looked at
            // again, which queues nothing twice.
            try store.setText(key: cursorKey, value: nil)
            try saveToken(store, lib.currentChangeToken)
            more = true
        }
        return more
    }

    /// Forgets where the looking was, as when backup is turned on again.
    static func restart(_ store: Store) throws {
        try store.set(key: tokenKey, value: nil)
        try store.setText(key: cursorKey, value: nil)
    }

    private static func saveToken(_ store: Store, _ t: PHPersistentChangeToken) throws {
        let data = try NSKeyedArchiver.archivedData(withRootObject: t, requiringSecureCoding: true)
        try store.set(key: tokenKey, value: data)
    }

    private static func loadToken(_ store: Store) -> PHPersistentChangeToken? {
        guard let data = (try? store.get(key: tokenKey)) ?? nil else { return nil }
        return try? NSKeyedUnarchiver.unarchivedObject(ofClass: PHPersistentChangeToken.self, from: data)
    }
}

extension Comparable {
    func clamped(to r: ClosedRange<Self>) -> Self { min(max(self, r.lowerBound), r.upperBound) }
}
