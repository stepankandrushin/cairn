// Thumbnails, at a cost that keeps to what is looked at: a photo this phone sent is shown from the
// library; another's from the thumbnail inside its first bytes, read by a range request, or, on Wi-Fi
// and for a file under 20 MB, the whole file made small; a video's from its first frame. Kept in Caches.

import AVFoundation
import ImageIO
import Network
import Photos
import SwiftUI
import UniformTypeIdentifiers

struct Thumb: View {
    let folder: String
    let entry: Entry
    @State private var image: UIImage?

    var body: some View {
        ZStack {
            Rectangle().fill(Color.secondary.opacity(0.15))
            if let image {
                Image(uiImage: image).resizable().scaledToFill()
            } else {
                Image(systemName: Media.isVideo(entry) ? "film" : Media.isImage(entry) ? "photo" : "doc")
                    .foregroundStyle(.secondary)
            }
            if Media.isVideo(entry) {
                Image(systemName: "play.circle.fill").foregroundStyle(.white).shadow(radius: 2)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomTrailing).padding(4)
            }
        }
        .task(id: entry.revision) {
            image = await Thumbnails.shared.image(folder, entry)
        }
    }
}

actor Thumbnails {
    static let shared = Thumbnails()
    static let side: CGFloat = 300
    private let memory = NSCache<NSString, UIImage>()
    private let path = NWPathMonitor()
    private var expensive = true

    init() {
        path.pathUpdateHandler = { p in Task { await Thumbnails.shared.set(expensive: p.isExpensive) } }
        path.start(queue: .global(qos: .utility))
    }

    private func set(expensive e: Bool) { expensive = e }

    static var dir: URL {
        FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0].appendingPathComponent("thumbs")
    }

    func image(_ folder: String, _ e: Entry) async -> UIImage? {
        guard Media.isMedia(e) else { return nil }
        let key = e.revision as NSString
        if let i = memory.object(forKey: key) { return i }
        let file = Thumbnails.dir.appendingPathComponent("\(e.revision).jpg")
        if let d = try? Data(contentsOf: file), let i = UIImage(data: d) {
            memory.setObject(i, forKey: key)
            return i
        }
        var made: UIImage?
        if let store = try? Cairn.store(), let asset = (try? store.assetOf(folder: folder, object: e.object)) ?? nil {
            made = await library(asset)
        }
        if made == nil, let f = try? await Folders.shared.folder(folder) {
            if Media.isVideo(e) {
                made = await video(f, e)
            } else {
                made = await embedded(f, e)
                if made == nil, !expensive, (e.size ?? .max) <= 20 * 1024 * 1024 {
                    made = await whole(f, e)
                }
            }
        }
        if let made {
            memory.setObject(made, forKey: key)
            try? FileManager.default.createDirectory(at: Thumbnails.dir, withIntermediateDirectories: true)
            try? made.jpegData(compressionQuality: 0.8)?.write(to: file)
        }
        return made
    }

    private func library(_ id: String) async -> UIImage? {
        guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [id], options: nil).firstObject else { return nil }
        let opts = PHImageRequestOptions()
        opts.deliveryMode = .highQualityFormat
        opts.isNetworkAccessAllowed = false
        return await withCheckedContinuation { c in
            PHImageManager.default().requestImage(for: asset, targetSize: CGSize(width: Thumbnails.side, height: Thumbnails.side),
                                                  contentMode: .aspectFill, options: opts) { img, _ in c.resume(returning: img) }
        }
    }

    private func fetch(_ f: Folder, _ e: Entry, range: Int?) async -> Data? {
        guard var r = try? f.contentRequest(object: e.object, revision: e.revision).urlRequest else { return nil }
        if let range { r.setValue("bytes=0-\(range - 1)", forHTTPHeaderField: "Range") }
        guard let (data, resp) = try? await URLSession.shared.data(for: r),
              let http = resp as? HTTPURLResponse, (200..<300).contains(http.statusCode) else { return nil }
        return data
    }

    /// The thumbnail a camera puts in the file, from its first bytes alone.
    private func embedded(_ f: Folder, _ e: Entry) async -> UIImage? {
        guard let data = await fetch(f, e, range: 256 * 1024),
              let src = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let opts: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageIfAbsent: false,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: Thumbnails.side,
        ]
        return CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary).map(UIImage.init(cgImage:))
    }

    private func whole(_ f: Folder, _ e: Entry) async -> UIImage? {
        guard let data = await fetch(f, e, range: nil),
              let src = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let opts: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: Thumbnails.side,
        ]
        return CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary).map(UIImage.init(cgImage:))
    }

    private func video(_ f: Folder, _ e: Entry) async -> UIImage? {
        guard let asset = try? await Streams.asset(f, e) else { return nil }
        let g = AVAssetImageGenerator(asset: asset)
        g.appliesPreferredTrackTransform = true
        g.maximumSize = CGSize(width: Thumbnails.side, height: Thumbnails.side)
        guard let (cg, _) = try? await g.image(at: .zero) else { return nil }
        return UIImage(cgImage: cg)
    }
}
