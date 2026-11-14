// The account, the folders shown in the Files app, what the Files app could not be shown and why, the
// cache, and signing out.

import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var model: Model
    @State private var cache: Int64 = 0

    var body: some View {
        NavigationStack {
            List {
                if let a = model.account {
                    Section("Account") {
                        LabeledContent("Server", value: a.server)
                        LabeledContent("User", value: a.user)
                        LabeledContent("This device", value: a.deviceName)
                        LabeledContent("Version", value: coreVersion())
                    }
                }
                Section {
                    ForEach(model.folders, id: \.folder) { f in
                        Toggle(f.name ?? f.folder, isOn: Binding(
                            get: { model.filesFolders.contains(f.folder) },
                            set: { on in
                                Task {
                                    do { try await model.showInFiles(f.folder, on) } catch { model.message = describe(error) }
                                }
                            }))
                    }
                } header: {
                    Text("In the Files app")
                } footer: {
                    Text("A file there is downloaded when it is opened, and kept only while the system has room, unless you choose Keep Downloaded. Files can add new files and folders; changing what is on the server is done here or on a computer.")
                }
                if !model.skipped.isEmpty {
                    Section {
                        ForEach(model.skipped, id: \.object) { s in
                            VStack(alignment: .leading) {
                                Text(s.display)
                                Text(s.reason).font(.footnote).foregroundStyle(.secondary)
                            }
                        }
                    } header: {
                        Text("Not shown in Files")
                    }
                }
                Section {
                    LabeledContent("Opened files and thumbnails", value: ByteCountFormatter.string(fromByteCount: cache, countStyle: .file))
                    Button("Clear") {
                        Cache.clear()
                        cache = Cache.size
                    }
                }
                Section {
                    Button("Sign out", role: .destructive) {
                        Task {
                            await Folders.shared.forget()
                            await model.signOut()
                        }
                    }
                } footer: {
                    Text("Photos not yet sent are not sent; what is on the server stays.")
                }
            }
            .navigationTitle("Settings")
            .onAppear {
                cache = Cache.size
                model.refresh()
            }
        }
    }
}
