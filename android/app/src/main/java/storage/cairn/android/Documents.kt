// The system's Files picker's view of a Cairn folder: each folder chosen in Settings a root, each
// folder listed when it is opened, each file downloaded when it is opened into a cache the system may
// empty, and new files and folders sent. This version changes nothing that is on the server: renaming,
// moving, deleting and writing over a file are not offered.

package storage.cairn.android

import android.content.res.AssetFileDescriptor
import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.Point
import android.net.Uri
import android.os.Bundle
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.webkit.MimeTypeMap
import kotlinx.coroutines.runBlocking
import uniffi.cairn_mobile.CairnException
import uniffi.cairn_mobile.Entry
import uniffi.cairn_mobile.EntryKind
import uniffi.cairn_mobile.Folder
import uniffi.cairn_mobile.FpItem
import uniffi.cairn_mobile.SkippedEntry
import uniffi.cairn_mobile.UploadState
import java.io.File
import java.io.FileNotFoundException
import java.util.concurrent.ConcurrentHashMap

class Documents : DocumentsProvider() {
    companion object {
        const val AUTHORITY = "storage.cairn.android.documents"

        private val ROOT_COLUMNS = arrayOf(
            Root.COLUMN_ROOT_ID, Root.COLUMN_DOCUMENT_ID, Root.COLUMN_TITLE, Root.COLUMN_SUMMARY,
            Root.COLUMN_FLAGS, Root.COLUMN_ICON,
        )
        private val DOC_COLUMNS = arrayOf(
            Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS, Document.COLUMN_SUMMARY,
        )

        /** A document's ID: its folder, then the object's hex, `root`, or `added:` and a file added here. */
        fun docId(folder: String, identifier: String) = Uri.encode(folder) + "/" + identifier

        fun split(docId: String): Pair<String, String> {
            val i = docId.indexOf('/')
            if (i < 0) throw FileNotFoundException("no document $docId")
            return Uri.decode(docId.substring(0, i)) to docId.substring(i + 1)
        }

        /** The roots changed: the folders shown, or signing in or out. */
        fun rootsChanged(context: android.content.Context) {
            context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(AUTHORITY), null)
        }

        /** The items last listed, so that a document asked for again is not asked of the server. */
        private val seen = ConcurrentHashMap<String, FpItem>()

        fun forget() {
            seen.clear()
        }
    }

    private lateinit var writes: Handler

    override fun onCreate(): Boolean {
        // A provider starts before the app's Application does: the core's TLS is set up here too.
        Native.ensure(context!!)
        writes = Handler(HandlerThread("cairn-documents").apply { start() }.looper)
        return true
    }

    private val ctx get() = context!!

    private fun folder(name: String): Folder = OpenFolders.get(ctx, name)

    private fun writable(folder: String) =
        Folders.load(Cairn.store(ctx)).firstOrNull { it.folder == folder }?.access.let { it == "write" || it == "manage" }

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val c = MatrixCursor(projection ?: ROOT_COLUMNS)
        val store = Cairn.store(ctx)
        if (Account.load(store) != null) {
            val infos = Folders.load(store)
            for (f in FilesFolders.load(store)) {
                val info = infos.firstOrNull { it.folder == f }
                var flags = Root.FLAG_SUPPORTS_IS_CHILD
                if (writable(f)) flags = flags or Root.FLAG_SUPPORTS_CREATE
                c.newRow()
                    .add(Root.COLUMN_ROOT_ID, f)
                    .add(Root.COLUMN_DOCUMENT_ID, docId(f, "root"))
                    .add(Root.COLUMN_TITLE, "Cairn")
                    .add(Root.COLUMN_SUMMARY, info?.name ?: f)
                    .add(Root.COLUMN_FLAGS, flags)
                    .add(Root.COLUMN_ICON, R.mipmap.ic_launcher)
            }
        }
        c.setNotificationUri(ctx.contentResolver, DocumentsContract.buildRootsUri(AUTHORITY))
        return c
    }

    private fun mime(name: String, dir: Boolean): String {
        if (dir) return Document.MIME_TYPE_DIR
        val ext = name.substringAfterLast('.', "").lowercase()
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }

    private fun row(c: MatrixCursor, folder: String, i: FpItem, id: String = docId(folder, i.identifier)) {
        val m = mime(i.filename, i.isDir)
        var flags = 0
        if (i.isDir && i.writable) flags = flags or Document.FLAG_DIR_SUPPORTS_CREATE
        if (!i.isDir && (m.startsWith("image/") || m.startsWith("video/"))) flags = flags or Document.FLAG_SUPPORTS_THUMBNAIL
        c.newRow()
            .add(Document.COLUMN_DOCUMENT_ID, id)
            .add(Document.COLUMN_DISPLAY_NAME, i.filename)
            .add(Document.COLUMN_MIME_TYPE, m)
            .add(Document.COLUMN_SIZE, if (i.isDir) null else i.size.toLong())
            .add(Document.COLUMN_LAST_MODIFIED, i.mtimeSec * 1000 + i.mtimeNsec.toLong() / 1_000_000)
            .add(Document.COLUMN_FLAGS, flags)
    }

    /** An item by identifier: from the last listing, or else asked of the server. */
    private fun item(folder: String, identifier: String): FpItem? {
        val id = docId(folder, identifier)
        seen[id]?.let { return it }
        return folder(folder).fpItem(identifier)?.also { seen[id] = it }
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val c = MatrixCursor(projection ?: DOC_COLUMNS)
        val (folder, identifier) = split(documentId)
        try {
            if (identifier.startsWith("added:")) {
                added(c, folder, identifier, documentId)
                return c
            }
            val i = item(folder, identifier) ?: throw FileNotFoundException("$documentId is gone")
            row(c, folder, i, documentId)
        } catch (e: CairnException) {
            throw FileNotFoundException(describe(e))
        }
        return c
    }

    /** A file added here: its copy until it is sent, then what the server has. */
    private fun added(c: MatrixCursor, folder: String, identifier: String, documentId: String) {
        val id = identifier.removePrefix("added:")
        val u = Cairn.store(ctx).byResource(Added.asset(id), Added.KIND, 0u)
        val done = u?.doneObject
        if (u != null && u.state == UploadState.DONE && done != null) {
            item(folder, done)?.let { row(c, folder, it, documentId); return }
        }
        val f = Added.file(ctx, id)
        val name = u?.name ?: u?.originalName ?: Added.name(id) ?: f.name
        c.newRow()
            .add(Document.COLUMN_DOCUMENT_ID, documentId)
            .add(Document.COLUMN_DISPLAY_NAME, name)
            .add(Document.COLUMN_MIME_TYPE, mime(name, false))
            .add(Document.COLUMN_SIZE, f.length())
            .add(Document.COLUMN_LAST_MODIFIED, f.lastModified())
            .add(Document.COLUMN_FLAGS, 0)
            .add(Document.COLUMN_SUMMARY, u?.error?.let { "Not sent: $it" } ?: "Waiting to be sent")
    }

    override fun queryChildDocuments(parentDocumentId: String, projection: Array<out String>?, sortOrder: String?): Cursor {
        val (folder, dir) = split(parentDocumentId)
        val c = MatrixCursor(projection ?: DOC_COLUMNS)
        try {
            val page = folder(folder).fpList(dir)
            for (i in page.items) {
                seen[docId(folder, i.identifier)] = i
                row(c, folder, i)
            }
            if (page.skipped.isNotEmpty()) {
                Cairn.store(ctx).noteSkipped(page.skipped.map { SkippedEntry(folder, it.`object`, it.display, it.reason) })
            }
        } catch (e: Exception) {
            // Said in the picker, not an empty folder (invariant 8).
            c.extras = Bundle().apply { putString(DocumentsContract.EXTRA_ERROR, describe(e)) }
        }
        c.setNotificationUri(ctx.contentResolver, DocumentsContract.buildChildDocumentsUri(AUTHORITY, parentDocumentId))
        return c
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean {
        val (pf, parent) = split(parentDocumentId)
        val (f, identifier) = split(documentId)
        if (pf != f) return false
        if (parent == "root") return true
        if (identifier.startsWith("added:")) return false
        return runCatching {
            folder(f).item(identifier)?.path?.any { it.`object` == parent } == true
        }.getOrDefault(false)
    }

    override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
        val (folder, identifier) = split(documentId)
        if (identifier.startsWith("added:")) {
            val id = identifier.removePrefix("added:")
            val f = Added.file(ctx, id)
            if (mode == "r") return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY)
            if (Added.name(id) == null) throw UnsupportedOperationException("it was written already")
            return ParcelFileDescriptor.open(f, ParcelFileDescriptor.parseMode(mode), writes) { e ->
                if (e == null) Added.written(ctx, id) else Added.abandoned(ctx, id)
            }
        }
        if (mode != "r") {
            throw UnsupportedOperationException("Cairn's folder does not change what is on the server yet: use the Cairn app or a computer")
        }
        try {
            val i = item(folder, identifier) ?: throw FileNotFoundException("$documentId is gone")
            if (i.isDir) throw FileNotFoundException("$documentId is a folder")
            val dest = Cache.file(ctx, folder(folder), i.identifier, revision(i), i.filename, i.size, cancelled = { signal?.isCanceled == true })
            return ParcelFileDescriptor.open(dest, ParcelFileDescriptor.MODE_READ_ONLY)
        } catch (e: CairnException) {
            throw FileNotFoundException(describe(e))
        }
    }

    /** An item's revision, whose bytes are its content version. */
    private fun revision(i: FpItem) = i.contentVersion.joinToString("") { "%02x".format(it) }

    override fun openDocumentThumbnail(documentId: String, sizeHint: Point?, signal: CancellationSignal?): AssetFileDescriptor {
        val (folder, identifier) = split(documentId)
        val i = item(folder, identifier) ?: throw FileNotFoundException("$documentId is gone")
        val entry = Entry(
            i.identifier, revision(i), EntryKind.FILE, i.filename.toByteArray(), i.filename, true, i.size,
            i.mtimeSec, i.mtimeNsec, i.mode, null,
        )
        val file = runBlocking { Thumbnails.file(ctx, folder, entry) } ?: throw FileNotFoundException("no thumbnail")
        val pfd = ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
        return AssetFileDescriptor(pfd, 0, file.length())
    }

    override fun createDocument(parentDocumentId: String, mimeType: String, displayName: String): String {
        val (folder, parent) = split(parentDocumentId)
        val name = displayName.replace('/', '_').ifEmpty { "untitled" }
        try {
            if (mimeType == Document.MIME_TYPE_DIR) {
                // A name taken is numbered, as the picker expects of a provider.
                var n = 1
                while (true) {
                    val candidate = if (n == 1) name else "$name ($n)"
                    try {
                        val made = folder(folder).mkdir(parent, candidate)
                        notifyChildren(parentDocumentId)
                        return docId(folder, made.`object`)
                    } catch (e: CairnException.Refused) {
                        if (e.status.toInt() != 409 || n >= 100) throw e
                        n++
                    }
                }
            }
            val withExt = if (name.contains('.')) name else {
                MimeTypeMap.getSingleton().getExtensionFromMimeType(mimeType)?.let { "$name.$it" } ?: name
            }
            val id = Added.create(ctx, folder, pathOf(folder, parent), withExt, parentDocumentId)
            return docId(folder, "added:$id")
        } catch (e: CairnException) {
            throw IllegalStateException(describe(e))
        }
    }

    /** A folder's path from the root, by its names, where a file added in it goes. */
    private fun pathOf(folder: String, dir: String): String {
        if (dir == "root") return ""
        val f = folder(folder)
        val root = f.list("root").dir.`object`
        return f.list(dir).path.filter { it.`object` != root }.joinToString("/") { it.display }
    }

    private fun notifyChildren(parentDocumentId: String) {
        ctx.contentResolver.notifyChange(DocumentsContract.buildChildDocumentsUri(AUTHORITY, parentDocumentId), null)
    }
}
