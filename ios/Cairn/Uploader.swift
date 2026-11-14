// The app's own uploads, beside the Photos extension's: while the app is open, when Upload now is
// tapped (and then on into the background, as a task the system shows), and overnight on power. Each
// resource is copied out of the library, iCloud's originals downloaded, and sent with the same request
// the extension would send, so that the server takes both as one file.

import BackgroundTasks
import Foundation
import os
import Photos

final class Uploader: @unchecked Sendable {
    static let shared = Uploader()

    private let state = OSAllocatedUnfairLock(initialState: (running: false, stopping: false))

    private var stopped: Bool {
        state.withLock { $0.stopping }
    }

    func stop() {
        state.withLock { $0.stopping = true }
    }

    /// Starts uploading what is queued, unless it is already. `all` takes what a Photos job waits to
    /// send too, as Upload now does: the server takes both as one file, and the job is then cancelled.
    func start(reason: String, all: Bool = false) {
        Task.detached(priority: .utility) { await self.run(progress: nil, all: all) }
    }

    /// Queues what is new and sends what is queued, until done or told to stop. `progress`, if given,
    /// is a background task's, which the system shows.
    func run(progress: Foundation.Progress?, all: Bool = false) async {
        let began = state.withLock { s -> Bool in
            if s.running { return false }
            s.running = true
            s.stopping = false
            return true
        }
        guard began else { return }
        defer { state.withLock { $0.running = false } }
        await MainActor.run { Model.shared.uploading = true }
        defer { Task { @MainActor in Model.shared.uploading = false; Model.shared.refresh() } }

        guard let store = try? Cairn.store(), let account = Account.load(store) else { return }
        let backup = Backup.load(store)
        guard backup.enabled else { return }
        do {
            while try PhotoLibrary.queueNew(store) {
                if stopped { return }
            }
            let folder = try account.session().folder(folder: backup.folder)
            var done: Int64 = 0
            while !stopped {
                // What no Photos job has, unless asked for all: a job's upload is the system's to send.
                let batch = try store.pending(limit: 50).filter { all || $0.job == nil }
                if batch.isEmpty { break }
                if let progress {
                    let s = try store.stats()
                    progress.totalUnitCount = done + Int64(s.planned + s.sent)
                }
                var sentAny = false
                for u in batch {
                    if stopped { break }
                    await note("Sending \(u.originalName)")
                    if try await send(u, store, folder) { sentAny = true }
                    done += 1
                    progress?.completedUnitCount = done
                    await MainActor.run { Model.shared.refresh() }
                }
                if !sentAny { break }
            }
            await note(nil)
        } catch {
            await note(describe(error))
        }
    }

    @MainActor private func note(_ s: String?) {
        Model.shared.uploadNote = s
    }

    /// One upload: true if the server answered for it, done or refused.
    private func send(_ u: Upload, _ store: Store, _ folder: Folder) async throws -> Bool {
        guard let resource = PhotoLibrary.resource(for: u) else {
            _ = try store.skip(id: u.id, reason: "it was deleted from the photo library before it was sent")
            return true
        }
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("upload-\(u.id)-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: tmp) }
        do {
            try await export(resource, to: tmp)
        } catch {
            _ = try store.notAnswered(id: u.id, reason: "the photo library could not give its bytes: \(describe(error))")
            return false
        }
        let result = try await Task.detached {
            try store.uploadNow(id: u.id, folder: folder, file: tmp.path, progress: nil)
        }.value
        return result.state == .done || result.state == .failed
    }

    private func export(_ r: PHAssetResource, to url: URL) async throws {
        let opts = PHAssetResourceRequestOptions()
        // An original only in iCloud is downloaded.
        opts.isNetworkAccessAllowed = true
        try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
            PHAssetResourceManager.default().writeData(for: r, toFile: url, options: opts) { error in
                if let error { c.resume(throwing: error) } else { c.resume() }
            }
        }
    }
}

/// What the system runs while the app is not open: a refresh now and then, uploads overnight on
/// power, and Upload now carried on into the background.
enum Background {
    static let refresh = "storage.cairn.ios.refresh"
    static let upload = "storage.cairn.ios.upload"
    static let uploadNow = "storage.cairn.ios.upload-now"

    static func register() {
        BGTaskScheduler.shared.register(forTaskWithIdentifier: refresh, using: nil) { task in
            schedule()
            let work = Task.detached {
                if let store = try? Cairn.store() {
                    _ = try? PhotoLibrary.queueNew(store)
                    Model.signalFiles(FilesDomains.load(store))
                }
                task.setTaskCompleted(success: true)
            }
            task.expirationHandler = { work.cancel() }
        }
        BGTaskScheduler.shared.register(forTaskWithIdentifier: upload, using: nil) { task in
            schedule()
            task.expirationHandler = { Uploader.shared.stop() }
            Task.detached {
                await Uploader.shared.run(progress: nil)
                task.setTaskCompleted(success: true)
            }
        }
        BGTaskScheduler.shared.register(forTaskWithIdentifier: uploadNow, using: nil) { task in
            guard let task = task as? BGContinuedProcessingTask else {
                task.setTaskCompleted(success: false)
                return
            }
            task.expirationHandler = { Uploader.shared.stop() }
            Task.detached {
                await Uploader.shared.run(progress: task.progress, all: true)
                task.setTaskCompleted(success: true)
            }
        }
    }

    static func schedule() {
        let r = BGAppRefreshTaskRequest(identifier: refresh)
        r.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(r)
        let p = BGProcessingTaskRequest(identifier: upload)
        p.requiresNetworkConnectivity = true
        p.requiresExternalPower = true
        try? BGTaskScheduler.shared.submit(p)
    }

    /// Upload now: a task the system shows and lets go on in the background, or, where it cannot be
    /// had, the uploads in the app while it is open.
    static func uploadNowInBackground() {
        let r = BGContinuedProcessingTaskRequest(identifier: uploadNow, title: "Backing up photos", subtitle: "Cairn")
        r.strategy = .queue
        do {
            try BGTaskScheduler.shared.submit(r)
        } catch {
            Uploader.shared.start(reason: "upload now", all: true)
        }
    }

    /// The Photos extension, which the system runs to send photos in the background, on or off.
    static func enablePhotosExtension(_ on: Bool) {
        let lib = PHPhotoLibrary.shared()
        if #available(iOS 27.0, *) {
            if on { try? lib.enableUploadJobExtension(with: nil) } else { try? lib.disableUploadJobExtension() }
        } else {
            try? lib.setUploadJobExtensionEnabled(on)
        }
    }

    static var photosExtensionEnabled: Bool {
        PHPhotoLibrary.shared().uploadJobExtensionEnabled
    }
}
