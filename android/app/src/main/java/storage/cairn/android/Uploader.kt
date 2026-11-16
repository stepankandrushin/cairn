// The uploads: while the app is open, when new photos are taken, now and then in the background on the
// network and power chosen, and at Upload now, which shows a notification and goes on with the app
// closed. Each photo or video is copied out of MediaStore as it was taken, its location kept, and sent
// with the request planned for it before it was first sent, so that one sent twice is one file.

package storage.cairn.android

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.ServiceInfo
import android.provider.DocumentsContract
import android.provider.MediaStore
import androidx.core.app.NotificationCompat
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.ForegroundInfo
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.OutOfQuotaPolicy
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import uniffi.cairn_mobile.AssetResource
import uniffi.cairn_mobile.Destination
import uniffi.cairn_mobile.Folder
import uniffi.cairn_mobile.NameStyle
import uniffi.cairn_mobile.Store
import uniffi.cairn_mobile.Upload
import uniffi.cairn_mobile.UploadState
import java.io.File
import java.io.IOException
import java.util.TimeZone
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

object Uploader {
    private val mutex = Mutex()
    @Volatile private var stopping = false

    /** Sending now, what is being sent, and a tick on each change, for the screens. */
    val uploading = MutableStateFlow(false)
    val note = MutableStateFlow<String?>(null)
    val changed = MutableStateFlow(0L)

    /** The run going on ends after the file it is sending. */
    fun stop() {
        stopping = true
    }

    /**
     * Queues what is new and sends what is queued, until done or `stop` says so; one run at a time, a
     * second waiting for the first. `progress` is told how many of how many. Without `send`, it only
     * queues.
     */
    suspend fun run(
        context: Context,
        stop: () -> Boolean = { false },
        send: Boolean = true,
        progress: ((Int, Int) -> Unit)? = null,
    ) = withContext(Dispatchers.IO) {
        mutex.withLock {
            stopping = false
            uploading.value = true
            try {
                body(context.applicationContext, { stopping || stop() }, progress, send)
            } catch (e: Exception) {
                note.value = describe(e)
            } finally {
                uploading.value = false
                changed.value++
            }
        }
    }

    private fun body(context: Context, stop: () -> Boolean, progress: ((Int, Int) -> Unit)?, sends: Boolean) {
        val store = Cairn.store(context)
        val account = Account.load(store) ?: return
        Added.sweep(context, store)
        val backup = Backup.load(store)
        if (backup.enabled) {
            note.value = "Looking for new photos"
            while (MediaLibrary.queueNew(context, store)) {
                changed.value++
                if (stop()) return
            }
            changed.value++
        }
        if (!sends) {
            note.value = null
            return
        }
        val session = account.session(store)
        val folders = mutableMapOf<String, Folder>()
        val tried = mutableSetOf<Long>()
        var done = 0
        File(context.cacheDir, "upload").deleteRecursively()
        while (!stop()) {
            // A photo waits while backup is off; a file added from another app goes whatever.
            val batch = store.pending(1000u)
                .filter { it.id !in tried && (backup.enabled || Added.isAdded(it)) }
                .take(50)
            if (batch.isEmpty()) break
            val s = store.stats()
            val total = done + (s.planned + s.sent).toInt()
            for (u in batch) {
                if (stop()) break
                note.value = "Sending ${u.originalName}"
                progress?.invoke(done, total)
                val folder = folders.getOrPut(u.folder) { session.folder(u.folder) }
                val answered = send(context, store, u, folder, stop)
                // Not answered: tried again on the next run, not this one.
                if (!answered) tried += u.id
                done++
                changed.value++
            }
        }
        note.value = null
    }

    /** One upload: true if the server answered for it, done or refused. */
    private fun send(context: Context, store: Store, u: Upload, folder: Folder, stop: () -> Boolean): Boolean {
        val source: File
        var copy: File? = null
        if (Added.isAdded(u)) {
            source = Added.file(context, u)
            if (!source.exists()) {
                store.skip(u.id, "its bytes were not kept on the phone: add it again")
                return true
            }
        } else {
            val uri = MediaLibrary.uri(u) ?: run {
                store.skip(u.id, "it is not a photo or video of this phone")
                return true
            }
            val from = MediaLibrary.folderOf(context, uri)
            if (from == null) {
                store.skip(u.id, "it was deleted from the phone before it was sent")
                return true
            }
            // Its folder no longer chosen since it was queued: not sent, unless it went once already.
            if (!Backup.load(store).wants(from) && store.forget(u.id)) return true
            val dir = File(context.cacheDir, "upload").apply { mkdirs() }
            copy = File(dir, "${u.id}-${UUID.randomUUID()}")
            try {
                // The original, its location as taken: without ACCESS_MEDIA_LOCATION Android would
                // take it out, and the copy on the server would not be the photo.
                val original = MediaStore.setRequireOriginal(uri)
                context.contentResolver.openInputStream(original).use { input ->
                    input ?: throw IOException("MediaStore gave no bytes")
                    copy.outputStream().use { input.copyTo(it, 1 shl 20) }
                }
            } catch (e: SecurityException) {
                copy.delete()
                store.notAnswered(u.id, "Cairn needs access to where photos were taken, so as to send them whole: allow it in the app's permissions")
                return false
            } catch (e: Exception) {
                copy.delete()
                store.notAnswered(u.id, "the phone could not give its bytes: ${describe(e)}")
                return false
            }
            source = copy
        }
        try {
            val r = store.uploadNow(u.id, folder, source.path, Watch(stop))
            if (Added.isAdded(r) && r.state == UploadState.DONE) Added.done(context, store, r)
            return r.state == UploadState.DONE || r.state == UploadState.FAILED
        } catch (e: Exception) {
            note.value = describe(e)
            return false
        } finally {
            copy?.delete()
        }
    }
}

/**
 * Files added in the Files picker or by another app through the document provider: kept in the app's
 * files once written, and queued as an upload like a photo, to the folder they were added in, so that
 * they are sent whatever happens to the app or the network meanwhile.
 */
object Added {
    private const val PREFIX = "added:"
    const val KIND = 0

    private data class Pending(val folder: String, val path: String, val name: String, val parentDoc: String)

    private val pending = ConcurrentHashMap<String, Pending>()

    fun isAdded(u: Upload) = u.asset.startsWith(PREFIX)

    fun asset(id: String) = PREFIX + id

    private fun dir(context: Context) = File(context.filesDir, "added").apply { mkdirs() }

    fun file(context: Context, id: String) = File(dir(context), id)

    fun file(context: Context, u: Upload) = file(context, u.asset.removePrefix(PREFIX))

    /** A new file `name` in `path` of `folder`, to be written: its ID. */
    fun create(context: Context, folder: String, path: String, name: String, parentDoc: String): String {
        val id = UUID.randomUUID().toString()
        file(context, id).createNewFile()
        pending[id] = Pending(folder, path, name, parentDoc)
        return id
    }

    fun name(id: String): String? = pending[id]?.name

    /** Its writer closed it: queued, durably, and sent. */
    fun written(context: Context, id: String) {
        val p = pending.remove(id) ?: return
        val store = Cairn.store(context)
        val f = file(context, id)
        val ms = f.lastModified().takeIf { it > 0 } ?: System.currentTimeMillis()
        store.setText("added.parent.$id", p.parentDoc)
        store.plan(
            Destination(p.folder, p.path, NameStyle.ORIGINAL, false),
            listOf(
                AssetResource(
                    asset(id), KIND, 0u, p.name, Math.floorDiv(ms, 1000L),
                    (Math.floorMod(ms, 1000L) * 1_000_000).toUInt(), TimeZone.getDefault().getOffset(ms) / 1000,
                ),
            ),
        )
        Background.sendAdded(context)
    }

    /** Its writer failed: nothing is sent. */
    fun abandoned(context: Context, id: String) {
        pending.remove(id)
        file(context, id).delete()
    }

    /** Sent: its copy here is let go, and the folder it went in is shown again. */
    fun done(context: Context, store: Store, u: Upload) {
        val id = u.asset.removePrefix(PREFIX)
        file(context, id).delete()
        val parent = store.getText("added.parent.$id")
        if (parent != null) {
            context.contentResolver.notifyChange(DocumentsContract.buildChildDocumentsUri(Documents.AUTHORITY, parent), null)
        }
    }

    /** A copy left a day without an upload of its own (the app ended between its writing and its queueing) is let go. */
    fun sweep(context: Context, store: Store) {
        val old = System.currentTimeMillis() - TimeUnit.DAYS.toMillis(1)
        for (f in dir(context).listFiles().orEmpty()) {
            if (f.lastModified() < old && !pending.containsKey(f.name) && store.byResource(asset(f.name), KIND, 0u) == null) {
                f.delete()
            }
        }
    }
}

/** What WorkManager runs: a run of the uploader, shown as a notification when asked for by Upload now. */
class BackupWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result {
        val now = inputData.getBoolean(NOW, false)
        if (now) runCatching { setForeground(info(0, 0)) }
        Uploader.run(applicationContext, stop = { isStopped }) { done, total ->
            if (now) setForegroundAsync(info(done, total))
        }
        if (Background.NEW_MEDIA in tags) Background.rearm(applicationContext) else Background.schedule(applicationContext)
        return Result.success()
    }

    override suspend fun getForegroundInfo(): ForegroundInfo = info(0, 0)

    private fun info(done: Int, total: Int): ForegroundInfo {
        Background.channel(applicationContext)
        val n = NotificationCompat.Builder(applicationContext, Background.CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("Backing up photos")
            .setContentText(if (total > 0) "$done of $total" else "Cairn")
            .setProgress(total, done, total == 0)
            .setOngoing(true)
            .setSilent(true)
            .build()
        return ForegroundInfo(NOTIFICATION, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    }

    companion object {
        const val NOW = "now"
        private const val NOTIFICATION = 1
    }
}

object Background {
    const val CHANNEL = "backup"
    private const val PERIODIC = "backup"
    const val NEW_MEDIA = "backup-new-media"
    private const val NOW = "upload-now"
    private const val ADDED = "send-added"

    fun channel(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Backup", NotificationManager.IMPORTANCE_LOW))
    }

    private fun constraints(b: Backup) = Constraints.Builder()
        .setRequiredNetworkType(if (b.wifiOnly) NetworkType.UNMETERED else NetworkType.CONNECTED)
        .setRequiresCharging(b.chargingOnly)
        .build()

    /**
     * While backup is on: a run every hour, and one a little after new photos or videos appear, each
     * on the network and power chosen; `replace` when the choice changed.
     */
    fun schedule(context: Context, replace: Boolean = false) {
        val wm = WorkManager.getInstance(context)
        val b = Backup.load(Cairn.store(context))
        if (!b.enabled) {
            wm.cancelUniqueWork(PERIODIC)
            wm.cancelUniqueWork(NEW_MEDIA)
            return
        }
        wm.enqueueUniquePeriodicWork(
            PERIODIC, ExistingPeriodicWorkPolicy.UPDATE,
            PeriodicWorkRequestBuilder<BackupWorker>(1, TimeUnit.HOURS).setConstraints(constraints(b)).build(),
        )
        enqueueNewMedia(context, b, if (replace) ExistingWorkPolicy.REPLACE else ExistingWorkPolicy.KEEP)
    }

    /** The run a content trigger started sets the next one, after itself: a trigger fires once. */
    fun rearm(context: Context) {
        val b = Backup.load(Cairn.store(context))
        if (b.enabled) enqueueNewMedia(context, b, ExistingWorkPolicy.APPEND_OR_REPLACE)
    }

    private fun enqueueNewMedia(context: Context, b: Backup, policy: ExistingWorkPolicy) {
        val media = Constraints.Builder()
            .setRequiredNetworkType(if (b.wifiOnly) NetworkType.UNMETERED else NetworkType.CONNECTED)
            .setRequiresCharging(b.chargingOnly)
            .addContentUriTrigger(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, true)
            .addContentUriTrigger(MediaStore.Video.Media.EXTERNAL_CONTENT_URI, true)
            .setTriggerContentUpdateDelay(10, TimeUnit.SECONDS)
            .setTriggerContentMaxDelay(2, TimeUnit.MINUTES)
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(
            NEW_MEDIA, policy,
            OneTimeWorkRequestBuilder<BackupWorker>().setConstraints(media).addTag(NEW_MEDIA).build(),
        )
    }

    /** Upload now: on any network, shown as a notification, and on with the app closed. */
    fun uploadNow(context: Context) {
        val r = OneTimeWorkRequestBuilder<BackupWorker>()
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setExpedited(OutOfQuotaPolicy.RUN_AS_NON_EXPEDITED_WORK_REQUEST)
            .setInputData(workDataOf(BackupWorker.NOW to true))
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(NOW, ExistingWorkPolicy.KEEP, r)
    }

    /** A file added from another app: sent as soon as there is a network. */
    fun sendAdded(context: Context) {
        val r = OneTimeWorkRequestBuilder<BackupWorker>()
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(ADDED, ExistingWorkPolicy.APPEND_OR_REPLACE, r)
    }

    fun cancelAll(context: Context) {
        val wm = WorkManager.getInstance(context)
        for (n in listOf(PERIODIC, NEW_MEDIA, NOW, ADDED)) wm.cancelUniqueWork(n)
    }
}
