// The Photos background upload extension: the system sends each queued photo's bytes itself, with the
// request the core built for it, while the app is closed and the phone locked, and calls this when it
// can: to read what came back, and to queue more.
//
// Each job is an upload the core planned, its request the same on every try, so the server takes a job
// sent twice, or a job and the app's own upload of the same photo, as one file.

import ExtensionFoundation
import Foundation
import Photos

@main
final class UploadExtension: PHBackgroundResourceUploadExtension {
    private let lock = NSLock()
    private var stopping = false

    required init() {}

    private var stopped: Bool {
        lock.lock(); defer { lock.unlock() }
        return stopping
    }

    func notifyTermination() {
        lock.lock(); stopping = true; lock.unlock()
    }

    func process() -> PHBackgroundResourceUploadProcessingResult {
        do {
            let store = try Cairn.store()
            guard let account = Account.load(store), Backup.load(store).enabled else {
                return .completed
            }
            let folder = try account.session().folder(folder: Backup.load(store).folder)
            try answered(store)
            if stopped { return .processing }
            let more = try PhotoLibrary.queueNew(store, limit: 200)
            if stopped { return .processing }
            let queuedMore = try queue(store, folder)
            return more || queuedMore ? .processing : .completed
        } catch let e as PHPhotosError where e.code == .limitExceeded {
            return .processing
        } catch {
            return .failure
        }
    }

    /// Jobs the system is done with. One the network failed is tried again as it is, while the system
    /// allows; one the server refused keeps the server's reason. Each is recorded before it is
    /// acknowledged, which frees its place and forgets it.
    private func answered(_ store: Store) throws {
        let lib = PHPhotoLibrary.shared()
        let retry = PHAssetResourceUploadJob.fetchJobs(action: .retry, options: nil)
        for i in 0..<retry.count {
            if stopped { return }
            let job = retry.object(at: i)
            if refused(job) { continue }
            try lib.performChangesAndWait {
                PHAssetResourceUploadJobChangeRequest(for: job)?.retry(destination: nil)
            }
        }
        let done = PHAssetResourceUploadJob.fetchJobs(action: .acknowledge, options: nil)
        for i in 0..<done.count {
            if stopped { return }
            let job = done.object(at: i)
            if let u = try store.byJob(job: job.localIdentifier) {
                let headers = (job.responseHeaderFields ?? [:]).map { Header(name: $0.key, value: $0.value) }
                if job.state == .succeeded {
                    _ = try store.finish(id: u.id, status: 201, headers: headers)
                } else if refused(job) {
                    _ = try store.finish(id: u.id, status: 409, headers: headers)
                } else {
                    // Given up on by the system: queued again, as the same request, for a new job.
                    let why = job.error?.localizedDescription ?? "the system did not send it"
                    _ = try store.notAnswered(id: u.id, reason: why)
                }
            }
            try lib.performChangesAndWait {
                PHAssetResourceUploadJobChangeRequest(for: job)?.acknowledge()
            }
        }
    }

    /// The server answered and refused, saying why.
    private func refused(_ job: PHAssetResourceUploadJob) -> Bool {
        job.responseHeaderFields?.keys.contains(where: { $0.lowercased() == "x-cairn-error" }) == true
    }

    /// New jobs for what is queued and has none, up to the system's limit: true if more wait. A job
    /// whose upload the app sent itself meanwhile is cancelled first.
    private func queue(_ store: Store, _ folder: Folder) throws -> Bool {
        let lib = PHPhotoLibrary.shared()
        var inFlight = 0
        if #available(iOS 26.5, *) {
            let waiting = PHAssetResourceUploadJob.fetchJobs(action: .process, options: nil)
            for i in 0..<waiting.count {
                let job = waiting.object(at: i)
                if let u = try store.byJob(job: job.localIdentifier), u.state == .done {
                    try lib.performChangesAndWait {
                        PHAssetResourceUploadJobChangeRequest(for: job)?.cancel()
                    }
                }
            }
            inFlight = PHAssetResourceUploadJob.fetchJobs(action: .process, options: nil).count
        }
        // Below 26.5 the jobs in flight cannot be counted: the system's limit, thrown, says when to stop.
        let room = max(0, PHAssetResourceUploadJob.jobLimit - inFlight)
        if room == 0 { return true }
        let pending = try store.pending(limit: UInt32(room) * 2).filter { $0.job == nil }
        var made = 0
        for u in pending {
            if stopped || made >= room { break }
            guard let resource = PhotoLibrary.resource(for: u) else {
                _ = try store.skip(id: u.id, reason: "it was deleted from the photo library before it was sent")
                continue
            }
            let request = try store.requestFor(id: u.id, folder: folder).urlRequest
            var jobID: String?
            try lib.performChangesAndWait {
                let r = PHAssetResourceUploadJobChangeRequest.creationRequestForJob(destination: request, resource: resource)
                jobID = r.placeholderForCreatedAssetResourceUploadJob?.localIdentifier
            }
            _ = try store.markSent(id: u.id, job: jobID)
            made += 1
        }
        return pending.count > made
    }
}
