// Files opened and their thumbnails, kept in the app's cache, which the system empties as it needs
// room: nothing else of the server is kept on the phone.

package storage.cairn.android

import android.content.ContentUris
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ImageDecoder
import android.graphics.Matrix
import android.media.MediaMetadataRetriever
import android.net.ConnectivityManager
import android.provider.MediaStore
import android.util.Log
import android.util.LruCache
import android.util.Size
import androidx.exifinterface.media.ExifInterface
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import uniffi.cairn_mobile.Entry
import uniffi.cairn_mobile.EntryKind
import uniffi.cairn_mobile.Folder
import uniffi.cairn_mobile.HttpRequest
import java.io.ByteArrayInputStream
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap

object Media {
    private val images = setOf("jpg", "jpeg", "heic", "heif", "png", "gif", "webp", "tif", "tiff", "dng", "bmp", "avif")
    private val videos = setOf("mov", "mp4", "m4v", "3gp", "avi", "mkv", "webm")

    fun ext(name: String) = name.substringAfterLast('.', "").lowercase()
    fun isImage(e: Entry) = e.kind == EntryKind.FILE && ext(e.display) in images
    fun isVideo(e: Entry) = e.kind == EntryKind.FILE && ext(e.display) in videos
    fun isMedia(e: Entry) = isImage(e) || isVideo(e)
}

/** The folders asked for so far, each asked of the server once, until signing out. */
object OpenFolders {
    private val open = ConcurrentHashMap<String, Folder>()

    fun get(context: Context, name: String): Folder = open.getOrPut(name) { Cairn.session(context).folder(name) }

    fun forget() = open.clear()
}

object Cache {
    private fun open(context: Context) = File(context.cacheDir, "open")

    /** A file's bytes at `revision`, downloaded unless they are here already. */
    fun file(
        context: Context, folder: Folder, obj: String, revision: String, name: String, size: ULong,
        cancelled: () -> Boolean = { false }, progress: (Long, Long) -> Unit = { _, _ -> },
    ): File {
        val at = File(open(context), revision)
        val dest = File(at, name.replace('/', '_'))
        if (dest.exists()) return dest
        at.mkdirs()
        folder.download(obj, revision, dest.path, size, Watch(cancelled, progress))
        return dest
    }

    fun size(context: Context): Long =
        listOf(open(context), Thumbnails.dir(context)).sumOf { d -> d.walkBottomUp().filter { it.isFile }.sumOf { it.length() } }

    fun clear(context: Context) {
        open(context).deleteRecursively()
        Thumbnails.clear(context)
    }
}

/** A request from the core, sent by the platform's own HTTP. */
fun HttpRequest.open(range: String? = null): HttpURLConnection {
    val c = URL(url).openConnection() as HttpURLConnection
    c.requestMethod = method
    for (h in headers) c.setRequestProperty(h.name, h.value)
    if (range != null) c.setRequestProperty("Range", range)
    c.connectTimeout = 15_000
    c.readTimeout = 30_000
    return c
}

/**
 * Thumbnails, at a cost that keeps to what is looked at: a photo this phone sent is shown from
 * MediaStore; another's from the thumbnail inside its first bytes, read by a range request, or, on a
 * network that is not metered and for a file under 20 MB, the whole file made small; a video's from
 * its first frame.
 */
object Thumbnails {
    const val SIDE = 300
    private const val TAG = "Cairn"
    private val memory = object : LruCache<String, Bitmap>(32 * 1024 * 1024) {
        override fun sizeOf(key: String, value: Bitmap) = value.byteCount
    }
    private val gate = Semaphore(4)

    fun dir(context: Context) = File(context.cacheDir, "thumbs")

    fun clear(context: Context) {
        memory.evictAll()
        dir(context).deleteRecursively()
    }

    suspend fun image(context: Context, folder: String, e: Entry): Bitmap? = withContext(Dispatchers.IO) {
        if (!Media.isMedia(e)) return@withContext null
        memory.get(e.revision)?.let { return@withContext it }
        val file = File(dir(context), "${e.revision}.jpg")
        if (file.exists()) {
            BitmapFactory.decodeFile(file.path)?.let { memory.put(e.revision, it); return@withContext it }
        }
        val made = gate.withPermit { make(context, folder, e) } ?: return@withContext null
        memory.put(e.revision, made)
        runCatching {
            dir(context).mkdirs()
            val tmp = File(dir(context), "${e.revision}.part")
            tmp.outputStream().use { made.compress(Bitmap.CompressFormat.JPEG, 80, it) }
            tmp.renameTo(file)
        }
        made
    }

    /** The thumbnail's file, made if need be, for the Files picker. */
    suspend fun file(context: Context, folder: String, e: Entry): File? {
        image(context, folder, e) ?: return null
        return File(dir(context), "${e.revision}.jpg").takeIf { it.exists() }
    }

    private fun make(context: Context, folder: String, e: Entry): Bitmap? {
        runCatching { Cairn.store(context).assetOf(folder, e.`object`) }.getOrNull()?.let { asset ->
            library(context, asset, Media.isVideo(e))?.let { return it }
        }
        val f = runCatching { OpenFolders.get(context, folder) }.getOrNull() ?: return null
        if (Media.isVideo(e)) return video(f, e)
        embedded(f, e)?.let { return it }
        val metered = context.getSystemService(ConnectivityManager::class.java).isActiveNetworkMetered
        if (!metered && (e.size ?: ULong.MAX_VALUE) <= 20uL * 1024uL * 1024uL) return whole(f, e)
        return null
    }

    private fun library(context: Context, asset: String, video: Boolean): Bitmap? = runCatching {
        val (volume, id) = asset.split(':', limit = 2)
        val base = if (video) MediaStore.Video.Media.getContentUri(volume) else MediaStore.Images.Media.getContentUri(volume)
        context.contentResolver.loadThumbnail(ContentUris.withAppendedId(base, id.toLong()), Size(SIDE, SIDE), null)
    }.getOrNull()

    private fun fetch(f: Folder, e: Entry, range: Int?): ByteArray? = runCatching {
        val c = f.contentRequest(e.`object`, e.revision).open(range?.let { "bytes=0-${it - 1}" })
        try {
            if (c.responseCode !in 200..299) {
                Log.w(TAG, "${e.display}: the server answered ${c.responseCode}")
                return null
            }
            c.inputStream.use { it.readBytes() }
        } finally {
            c.disconnect()
        }
    }.onFailure { Log.w(TAG, "${e.display}: ${describe(it)}") }.getOrNull()

    /** The thumbnail a camera puts in the file, from its first bytes alone. */
    private fun embedded(f: Folder, e: Entry): Bitmap? {
        val data = fetch(f, e, 256 * 1024) ?: return null
        return runCatching {
            val exif = ExifInterface(ByteArrayInputStream(data))
            val t = exif.thumbnailBitmap ?: return null
            val turn = exif.rotationDegrees
            if (turn == 0) t else Bitmap.createBitmap(t, 0, 0, t.width, t.height, Matrix().apply { postRotate(turn.toFloat()) }, true)
        }.getOrNull()
    }

    private fun whole(f: Folder, e: Entry): Bitmap? {
        val data = fetch(f, e, null) ?: return null
        return runCatching {
            ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(data))) { d, info, _ ->
                val scale = SIDE.toFloat() / minOf(info.size.width, info.size.height)
                if (scale < 1) d.setTargetSize((info.size.width * scale).toInt().coerceAtLeast(1), (info.size.height * scale).toInt().coerceAtLeast(1))
                d.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
            }
        }.getOrNull()
    }

    private fun video(f: Folder, e: Entry): Bitmap? = runCatching {
        val req = f.contentRequest(e.`object`, e.revision)
        val r = MediaMetadataRetriever()
        try {
            r.setDataSource(req.url, req.headers.associate { it.name to it.value })
            r.getScaledFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST_SYNC, SIDE, SIDE)
        } finally {
            r.release()
        }
    }.getOrNull()
}
