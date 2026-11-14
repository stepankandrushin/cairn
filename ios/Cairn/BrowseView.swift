// The server's folders, live: each listed when opened, a photo or video shown by its thumbnail, and a
// file downloaded only when it is opened, into a cache the system may empty.

import AVKit
import QuickLook
import SwiftUI

/// The folders asked for so far, each asked of the server once.
actor Folders {
    static let shared = Folders()
    private var open: [String: Folder] = [:]

    func folder(_ name: String) throws -> Folder {
        if let f = open[name] { return f }
        guard let store = try? Cairn.store(), let account = Account.load(store) else {
            throw CairnError.Refused(status: 401, reason: "not signed in")
        }
        let f = try account.session().folder(folder: name)
        open[name] = f
        return f
    }

    func forget() { open = [:] }
}

struct BrowseView: View {
    @EnvironmentObject var model: Model

    var body: some View {
        NavigationStack {
            List(model.folders, id: \.folder) { f in
                NavigationLink(value: Place(folder: f.folder, dir: "root", title: f.name ?? f.folder)) {
                    Label(f.name ?? f.folder, systemImage: "folder")
                }
            }
            .navigationTitle("Browse")
            .navigationDestination(for: Place.self) { FolderView(place: $0) }
            .refreshable { await model.loadFolders() }
        }
    }
}

struct Place: Hashable {
    var folder: String
    var dir: String
    var title: String
}

enum Media {
    static let images: Set<String> = ["jpg", "jpeg", "heic", "heif", "png", "gif", "webp", "tif", "tiff", "dng", "bmp"]
    static let videos: Set<String> = ["mov", "mp4", "m4v", "3gp", "avi", "mkv", "webm"]

    static func ext(_ name: String) -> String { (name as NSString).pathExtension.lowercased() }
    static func isImage(_ e: Entry) -> Bool { e.kind == .file && images.contains(ext(e.display)) }
    static func isVideo(_ e: Entry) -> Bool { e.kind == .file && videos.contains(ext(e.display)) }
    static func isMedia(_ e: Entry) -> Bool { isImage(e) || isVideo(e) }
}

struct FolderView: View {
    let place: Place
    @State private var listing: Listing?
    @State private var error: String?
    @State private var grid: Bool?
    @State private var opening: Entry?

    var body: some View {
        Group {
            if let listing {
                let entries = listing.entries
                if grid ?? (entries.filter(Media.isMedia).count * 2 > entries.count) {
                    ScrollView {
                        LazyVGrid(columns: [GridItem(.adaptive(minimum: 110), spacing: 2)], spacing: 2) {
                            ForEach(entries, id: \.object) { e in cell(e) }
                        }
                    }
                } else {
                    List(entries, id: \.object) { e in row(e) }
                }
            } else if let error {
                ContentUnavailableView("Not listed", systemImage: "exclamationmark.triangle", description: Text(error))
            } else {
                ProgressView()
            }
        }
        .navigationTitle(place.title)
        .toolbar {
            Button {
                grid = !(grid ?? true)
            } label: {
                Image(systemName: (grid ?? true) ? "list.bullet" : "square.grid.3x3")
            }
        }
        .task { await load() }
        .refreshable { await load() }
        .sheet(item: Binding(get: { opening.map(Opened.init) }, set: { opening = $0?.entry })) { o in
            PreviewView(folder: place.folder, entry: o.entry)
        }
    }

    @ViewBuilder private func cell(_ e: Entry) -> some View {
        if e.kind == .dir {
            NavigationLink(value: Place(folder: place.folder, dir: e.object, title: e.display)) {
                VStack {
                    Image(systemName: "folder.fill").font(.largeTitle)
                    Text(e.display).font(.caption).lineLimit(2)
                }
                .frame(minWidth: 110, minHeight: 110)
            }
        } else {
            Button { opening = e } label: {
                Thumb(folder: place.folder, entry: e)
                    .frame(minWidth: 110, minHeight: 110)
                    .clipped()
            }
            .accessibilityIdentifier("entry-\(e.display)")
        }
    }

    @ViewBuilder private func row(_ e: Entry) -> some View {
        if e.kind == .dir {
            NavigationLink(value: Place(folder: place.folder, dir: e.object, title: e.display)) {
                Label(e.display, systemImage: "folder")
            }
        } else {
            Button { opening = e } label: {
                HStack {
                    if Media.isMedia(e) {
                        Thumb(folder: place.folder, entry: e).frame(width: 44, height: 44).clipped()
                    } else {
                        Image(systemName: e.kind == .symlink ? "arrow.turn.up.right" : "doc").frame(width: 44)
                    }
                    VStack(alignment: .leading) {
                        Text(e.display).foregroundStyle(.primary)
                        Text(detail(e)).font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            .disabled(e.kind == .symlink)
            .accessibilityIdentifier("entry-\(e.display)")
        }
    }

    private func detail(_ e: Entry) -> String {
        if e.kind == .symlink { return "link to \(e.target ?? "")" }
        let size = ByteCountFormatter.string(fromByteCount: Int64(e.size ?? 0), countStyle: .file)
        let date = Date(timeIntervalSince1970: Double(e.mtimeSec))
        return "\(size), \(date.formatted(date: .abbreviated, time: .shortened))"
    }

    private func load() async {
        do {
            let f = try await Folders.shared.folder(place.folder)
            let dir = place.dir
            listing = try await Task.detached { try f.list(dir: dir) }.value
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}

struct Opened: Identifiable {
    let entry: Entry
    var id: String { entry.object }
}

/// A file opened: a video streamed, a photo downloaded and shown, anything else downloaded and shown by
/// Quick Look.
struct PreviewView: View {
    let folder: String
    let entry: Entry
    @Environment(\.dismiss) private var dismiss
    @State private var url: URL?
    @State private var image: UIImage?
    @State private var player: AVPlayer?
    @State private var quickLook: URL?
    @State private var error: String?
    @State private var progress = Foundation.Progress(totalUnitCount: 1)

    var body: some View {
        NavigationStack {
            Group {
                if let player {
                    VideoPlayer(player: player).onAppear { player.play() }
                } else if let image {
                    ScrollView([.horizontal, .vertical]) {
                        Image(uiImage: image).resizable().scaledToFit()
                            .containerRelativeFrame([.horizontal, .vertical])
                    }
                    .accessibilityIdentifier("photo")
                } else if let url {
                    VStack(spacing: 16) {
                        Image(systemName: "doc").font(.largeTitle)
                        Button("Open") { quickLook = url }
                    }
                } else if let error {
                    ContentUnavailableView("Not opened", systemImage: "exclamationmark.triangle", description: Text(error))
                } else {
                    ProgressView(progress).padding()
                }
            }
            .navigationTitle(entry.display)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { progress.cancel(); dismiss() } }
                if let url {
                    ToolbarItem(placement: .primaryAction) { ShareLink(item: url) }
                }
            }
            .quickLookPreview($quickLook)
        }
        .task { await open() }
    }

    private func open() async {
        do {
            let f = try await Folders.shared.folder(folder)
            if Media.isVideo(entry), let item = try await Streams.item(f, entry) {
                player = AVPlayer(playerItem: item)
                return
            }
            let got = try await Cache.file(f, entry, progress)
            if Media.isImage(entry), let i = await Task.detached(operation: { UIImage(contentsOfFile: got.path)?.preparingForDisplay() }).value {
                image = i
            } else {
                quickLook = got
            }
            url = got
        } catch CairnError.Cancelled {
        } catch {
            self.error = describe(error)
        }
    }
}

/// Files opened, kept in Caches, which the system empties as it needs room: nothing else is kept.
enum Cache {
    static var dir: URL {
        FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0].appendingPathComponent("open")
    }

    static func file(_ f: Folder, _ e: Entry, _ progress: Foundation.Progress) async throws -> URL {
        let at = dir.appendingPathComponent(e.revision)
        let dest = at.appendingPathComponent(e.display.replacingOccurrences(of: "/", with: "_"))
        if FileManager.default.fileExists(atPath: dest.path) { return dest }
        try FileManager.default.createDirectory(at: at, withIntermediateDirectories: true)
        progress.totalUnitCount = Int64(max(e.size ?? 1, 1))
        let watch = Watch(progress)
        let (object, revision, size) = (e.object, e.revision, e.size ?? 0)
        _ = try await Task.detached {
            try f.download(object: object, revision: revision, dest: dest.path, size: size, progress: watch)
        }.value
        return dest
    }

    static var size: Int64 {
        let urls = FileManager.default.enumerator(at: dir.deletingLastPathComponent(), includingPropertiesForKeys: [.fileSizeKey])
        var n: Int64 = 0
        while let u = urls?.nextObject() as? URL {
            n += Int64((try? u.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0)
        }
        return n
    }

    static func clear() {
        try? FileManager.default.removeItem(at: dir)
        try? FileManager.default.removeItem(at: Thumbnails.dir)
    }
}

/// A video streamed from the server by a player that sends the web session's cookie.
enum Streams {
    private static var cookie: (String, Date)?

    static func item(_ f: Folder, _ e: Entry) async throws -> AVPlayerItem? {
        let req = try f.contentRequest(object: e.object, revision: e.revision)
        guard let url = URL(string: req.url), let host = url.host else { return nil }
        let value: String
        if let (v, at) = cookie, at.timeIntervalSinceNow > -24 * 3600 {
            value = v
        } else {
            guard let store = try? Cairn.store(), let s = try Account.load(store)?.session() else { return nil }
            value = try await Task.detached { try s.sessionCookie() }.value
            cookie = (value, Date())
        }
        var props: [HTTPCookiePropertyKey: Any] = [.name: "cairn", .value: value, .domain: host, .path: "/"]
        if url.scheme == "https" { props[.secure] = "TRUE" }
        guard let c = HTTPCookie(properties: props) else { return nil }
        let asset = AVURLAsset(url: url, options: [AVURLAssetHTTPCookiesKey: [c]])
        return AVPlayerItem(asset: asset)
    }

    static func asset(_ f: Folder, _ e: Entry) async throws -> AVURLAsset? {
        try await item(f, e)?.asset as? AVURLAsset
    }
}
