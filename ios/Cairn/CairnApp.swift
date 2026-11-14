import SwiftUI

@main
struct CairnApp: App {
    @StateObject private var model = Model.shared
    @Environment(\.scenePhase) private var phase

    init() {
        Background.register()
    }

    var body: some Scene {
        WindowGroup {
            Group {
                if model.account == nil {
                    SignInView()
                } else {
                    MainView()
                }
            }
            .environmentObject(model)
            .alert("Cairn", isPresented: Binding(get: { model.message != nil }, set: { if !$0 { model.message = nil } })) {
                Button("OK") { model.message = nil }
            } message: {
                Text(model.message ?? "")
            }
        }
        .onChange(of: phase) { _, now in
            switch now {
            case .active:
                model.refresh()
                Watcher.shared.restart()
                if model.backup.enabled { Uploader.shared.start(reason: "opened") }
            case .background:
                Watcher.shared.stop()
                Background.schedule()
            default:
                break
            }
        }
    }
}

struct MainView: View {
    @EnvironmentObject var model: Model

    var body: some View {
        TabView {
            PhotosView()
                .tabItem { Label("Photos", systemImage: "photo.on.rectangle.angled") }
            BrowseView()
                .tabItem { Label("Browse", systemImage: "folder") }
            SettingsView()
                .tabItem { Label("Settings", systemImage: "gearshape") }
        }
        .task { await model.loadFolders() }
    }
}

struct SignInView: View {
    @EnvironmentObject var model: Model
    @State private var server = "https://"
    @State private var token = ""
    @State private var deviceName = "iPhone"
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Server", text: $server)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("server")
                    SecureField("Token", text: $token)
                        .accessibilityIdentifier("token")
                    Button("Paste token") { token = UIPasteboard.general.string ?? token }
                } header: {
                    Text("Your server")
                } footer: {
                    Text("A token is made on the server, one per device: cairn-server token add <user> --label iphone")
                }
                Section {
                    TextField("This device's name", text: $deviceName)
                        .accessibilityIdentifier("deviceName")
                } footer: {
                    Text("Shown in each file's history, and the name of its photos' folder.")
                }
                Section {
                    Button {
                        Task { await signIn() }
                    } label: {
                        if busy { ProgressView() } else { Text("Sign in") }
                    }
                    .disabled(busy || token.isEmpty || deviceName.isEmpty)
                    .accessibilityIdentifier("signIn")
                    if let error {
                        Text(error).foregroundStyle(.red).accessibilityIdentifier("signInError")
                    }
                }
            }
            .navigationTitle("Cairn")
        }
    }

    private func signIn() async {
        busy = true
        defer { busy = false }
        do {
            try await model.signIn(server: server, token: token, deviceName: deviceName)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}
