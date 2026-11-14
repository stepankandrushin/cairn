// Backup of the photo library: where it goes and what it is called, how far it is, and what failed and
// why, each with Retry.

import Photos
import SwiftUI

struct PhotosView: View {
    @EnvironmentObject var model: Model

    var body: some View {
        NavigationStack {
            Group {
                if model.backup.enabled {
                    BackupStatus()
                } else {
                    BackupSetup()
                }
            }
            .navigationTitle("Photos")
        }
    }
}

struct BackupSetup: View {
    @EnvironmentObject var model: Model
    @State private var b = Backup()
    @State private var busy = false

    var body: some View {
        Form {
            Section {
                Picker("Folder", selection: $b.folder) {
                    ForEach(model.folders.filter { $0.access != "read" }, id: \.folder) { f in
                        Text(f.name ?? f.folder).tag(f.folder)
                    }
                }
                TextField("Path in it", text: $b.path)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier("backupPath")
            } header: {
                Text("Where they go")
            }
            Section {
                Picker("Names", selection: $b.originalNames) {
                    Text("Date and time, as Nextcloud named them").tag(false)
                    Text("As on the phone (IMG_0003.HEIC)").tag(true)
                }
                Toggle("A folder for each year and month", isOn: $b.subfolders)
            } footer: {
                Text(example)
            }
            Section {
                Picker("Back up", selection: $b.everything) {
                    Text("New photos and videos, from now on").tag(false)
                    Text("The whole library").tag(true)
                }
                .pickerStyle(.inline)
                .labelsHidden()
            } footer: {
                Text("Originals are sent as they are: HEIC, MOV, RAW. A Live Photo's video goes beside its photo. A photo deleted from the phone stays on the server.")
            }
            Section {
                Button {
                    Task {
                        busy = true
                        defer { busy = false }
                        do { try await model.startBackup(b) } catch { model.message = describe(error) }
                    }
                } label: {
                    if busy { ProgressView() } else { Text("Turn on backup") }
                }
                .disabled(busy || b.folder.isEmpty || b.path.isEmpty)
                .accessibilityIdentifier("startBackup")
            }
        }
        .onAppear { b = model.backup }
    }

    private var example: String {
        let name = b.originalNames ? "IMG_0003.HEIC" : {
            let f = DateFormatter()
            f.dateFormat = "yy-MM-dd HH-mm-ss"
            return "\(f.string(from: Date())) 0003.heic"
        }()
        var parts = [b.path]
        if b.subfolders {
            let f = DateFormatter()
            f.dateFormat = "yyyy/MM"
            parts.append(f.string(from: Date()))
        }
        parts.append(name)
        return "For example: " + parts.joined(separator: "/")
    }
}

struct BackupStatus: View {
    @EnvironmentObject var model: Model

    var body: some View {
        List {
            Section {
                LabeledContent("Folder", value: "\(model.title(model.backup.folder))/\(model.backup.path)")
                LabeledContent("Sent", value: "\(model.stats.done)").accessibilityIdentifier("sentCount")
                LabeledContent("Waiting", value: "\(model.stats.planned + model.stats.sent)")
                    .accessibilityIdentifier("waitingCount")
                if model.stats.failed + model.stats.skipped > 0 {
                    LabeledContent("Not sent", value: "\(model.stats.failed + model.stats.skipped)")
                }
                if let note = model.uploadNote {
                    Text(note).font(.footnote).foregroundStyle(.secondary)
                }
                Button {
                    Background.uploadNowInBackground()
                } label: {
                    HStack {
                        Text("Upload now")
                        if model.uploading { Spacer(); ProgressView() }
                    }
                }
                .accessibilityIdentifier("uploadNow")
            } footer: {
                Text(Background.photosExtensionEnabled
                    ? "The system sends new photos in the background, on its own schedule; Upload now sends what waits at once."
                    : "Photos are sent while Cairn is open, overnight on power, and when you tap Upload now.")
            }
            if !model.failed.isEmpty {
                Section {
                    ForEach(model.failed, id: \.id) { u in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(u.originalName)
                            Text(u.error ?? "").font(.footnote).foregroundStyle(.red)
                            Button("Retry") { model.retry(u) }.font(.footnote)
                        }
                    }
                    Button("Retry all") { model.retryAll() }
                } header: {
                    Text("Not sent")
                }
            }
            if !model.recent.isEmpty {
                Section("Last sent") {
                    ForEach(model.recent, id: \.id) { u in
                        VStack(alignment: .leading) {
                            Text(u.name ?? u.path)
                            if u.name.map({ !u.path.hasSuffix($0) }) ?? false {
                                Text("Its name was taken by other bytes").font(.footnote).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
            Section {
                Button("Turn off backup", role: .destructive) {
                    Task { await model.stopBackup() }
                }
            } footer: {
                Text("What was sent stays on the server.")
            }
        }
        .refreshable { model.refresh() }
    }
}
