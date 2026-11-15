// The app's state: the account signed in, its folders, the backup's settings and how far it is.

package storage.cairn.android

import android.content.Context
import android.net.ConnectivityManager
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.cairn_mobile.CairnException
import uniffi.cairn_mobile.FolderInfo
import uniffi.cairn_mobile.Session
import uniffi.cairn_mobile.SkippedEntry
import uniffi.cairn_mobile.Store
import uniffi.cairn_mobile.Upload
import uniffi.cairn_mobile.UploadStats
import uniffi.cairn_mobile.newDeviceId

object Model {
    private lateinit var app: Context
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    var account by mutableStateOf<Account?>(null)
    var folders by mutableStateOf<List<FolderInfo>>(emptyList())
    var backup by mutableStateOf(Backup())
    var stats by mutableStateOf(UploadStats(0uL, 0uL, 0uL, 0uL, 0uL))
    var failed by mutableStateOf<List<Upload>>(emptyList())
    var recent by mutableStateOf<List<Upload>>(emptyList())
    var filesFolders by mutableStateOf<List<String>>(emptyList())
    var skipped by mutableStateOf<List<SkippedEntry>>(emptyList())
    var message by mutableStateOf<String?>(null)
    var uploading by mutableStateOf(false)
    var uploadNote by mutableStateOf<String?>(null)

    private var started = false

    fun init(context: Context) {
        if (started) return
        started = true
        app = context.applicationContext
        val store = store()
        account = Account.load(store)
        backup = Backup.load(store)
        folders = Folders.load(store)
        filesFolders = FilesFolders.load(store)
        scope.launch { Uploader.changed.collect { refresh() } }
        scope.launch { Uploader.uploading.collect { uploading = it } }
        scope.launch { Uploader.note.collect { uploadNote = it } }
    }

    private fun store(): Store = Cairn.store(app)

    /** Signs in, checking the token with the server first; makes this device's ID the first time. */
    suspend fun signIn(server: String, token: String, deviceName: String) {
        val store = store()
        val server = server.trim()
        val token = token.trim()
        val device = store.getText("device") ?: newDeviceId()
        val allowHttp = server.startsWith("http://")
        val me = withContext(Dispatchers.IO) { Session(server, token, device, deviceName, allowHttp).me() }
        withContext(Dispatchers.IO) {
            store.setText("device", device)
            Vault.save(store, token)
        }
        val a = Account(server, device, deviceName.trim(), me.user, allowHttp)
        a.save(store)
        Folders.save(store, me.folders)
        account = a
        folders = me.folders
        if (backup.folder.isEmpty()) {
            me.folders.firstOrNull { it.access != "read" }?.let {
                backup = backup.copy(folder = it.folder, path = "Photos/${a.deviceName}")
                backup.save(store)
            }
        }
        if (filesFolders.isEmpty()) me.folders.firstOrNull()?.let { showInFiles(it.folder, true) }
        Documents.rootsChanged(app)
    }

    suspend fun signOut() {
        stopBackup()
        Background.cancelAll(app)
        withContext(Dispatchers.IO) {
            val store = store()
            store.forgetAll()
            Vault.delete(store)
            Cache.clear(app)
        }
        OpenFolders.forget()
        Documents.forget()
        account = null
        folders = emptyList()
        backup = Backup()
        filesFolders = emptyList()
        Documents.rootsChanged(app)
        refresh()
    }

    suspend fun loadFolders() {
        if (account == null) return
        try {
            val got = withContext(Dispatchers.IO) { Cairn.session(app).me().folders }
            folders = got
            Folders.save(store(), got)
        } catch (e: Exception) {
            message = describe(e)
        }
    }

    fun title(folder: String): String = folders.firstOrNull { it.folder == folder }?.name ?: folder

    /** Where the counts and lists stand now. */
    fun refresh() {
        scope.launch {
            val got = withContext(Dispatchers.IO) {
                runCatching {
                    val s = store()
                    listOf(s.stats(), s.failed(200u), s.recent(30u), s.skipped())
                }.getOrNull()
            } ?: return@launch
            @Suppress("UNCHECKED_CAST")
            run {
                stats = got[0] as UploadStats
                failed = got[1] as List<Upload>
                recent = got[2] as List<Upload>
                skipped = got[3] as List<SkippedEntry>
            }
        }
    }

    /** The app came to the screen: what waits is sent while it is open, on the network chosen. */
    fun opened() {
        if (!started) return
        refresh()
        if (account == null || !backup.enabled) return
        Background.schedule(app)
        val metered = app.getSystemService(ConnectivityManager::class.java).isActiveNetworkMetered
        if (!backup.wifiOnly || !metered) scope.launch { Uploader.run(app) }
    }

    // Backup

    suspend fun startBackup(b: Backup) {
        if (!MediaLibrary.authorized(app)) {
            throw CairnException.Failed(
                "Cairn needs access to all photos and videos, and to where they were taken, to back them up: allow it in the app's permissions",
            )
        }
        val now = b.copy(enabled = true, since = System.currentTimeMillis() / 1000)
        withContext(Dispatchers.IO) {
            val store = store()
            MediaLibrary.restart(store)
            now.save(store)
        }
        backup = now
        Background.schedule(app, replace = true)
        scope.launch { Uploader.run(app) }
    }

    /** A choice of when, changed while backup is on. */
    fun saveBackup(b: Backup) {
        backup = b
        b.save(store())
        Background.schedule(app, replace = true)
    }

    fun stopBackup() {
        val b = backup.copy(enabled = false)
        b.save(store())
        backup = b
        Uploader.stop()
        Background.schedule(app)
    }

    fun uploadNow() {
        Background.uploadNow(app)
    }

    fun retry(u: Upload) {
        runCatching { store().retry(u.id) }
        refresh()
        Background.uploadNow(app)
    }

    fun retryAll() {
        for (u in failed) runCatching { store().retry(u.id) }
        refresh()
        Background.uploadNow(app)
    }

    // The Files picker

    fun showInFiles(folder: String, on: Boolean) {
        filesFolders = if (on) (filesFolders + folder).distinct() else filesFolders - folder
        FilesFolders.save(store(), filesFolders)
        Documents.rootsChanged(app)
    }
}
