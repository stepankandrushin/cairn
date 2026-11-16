// What of the phone's photos and videos is backed up: each found through MediaStore, first what was
// there (all of it, or what was taken since backup was turned on), then what each volume's generation
// number says changed since the last look, and queued in the core's database, once each, before
// anything is sent. Only those in the folders chosen (Backup.wants): every row is looked at, so that
// a folder chosen later is found by going through them again from the start.
//
// A changed row, not only an added one: a camera writes a photo as pending, which no other app sees,
// and publishes it later, when its generation_added is still the one it had while hidden; publishing
// moves its generation_modified past anything seen before.

package storage.cairn.android

import android.Manifest
import android.content.ContentResolver
import android.content.ContentUris
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.MediaStore
import android.util.Log
import org.json.JSONObject
import uniffi.cairn_mobile.AssetResource
import uniffi.cairn_mobile.Store
import uniffi.cairn_mobile.Upload
import uniffi.cairn_mobile.UploadState
import java.util.TimeZone

object MediaLibrary {
    /** An upload's `kind`: MediaStore's media type. */
    const val KIND_IMAGE = MediaStore.Files.FileColumns.MEDIA_TYPE_IMAGE
    const val KIND_VIDEO = MediaStore.Files.FileColumns.MEDIA_TYPE_VIDEO

    private const val TAG = "Cairn"
    private const val STATE_KEY = "media.volumes"
    private const val RESTART_KEY = "media.restart"
    private const val BATCH = 500

    /** What backup asks for: the photos and videos, and their location as taken, not redacted. */
    val permissions: Array<String>
        get() = if (Build.VERSION.SDK_INT >= 33) {
            arrayOf(Manifest.permission.READ_MEDIA_IMAGES, Manifest.permission.READ_MEDIA_VIDEO, Manifest.permission.ACCESS_MEDIA_LOCATION)
        } else {
            arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE, Manifest.permission.ACCESS_MEDIA_LOCATION)
        }

    fun authorized(context: Context): Boolean =
        permissions.all { context.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

    /**
     * Where a volume's looking stands: its database's version, its generation when backup began, the
     * generation seen to, and the first pass's last ID, or -1 once it is done.
     */
    private data class Volume(val version: String, val first: Long, val generation: Long, val cursor: Long)

    private fun loadState(store: Store): MutableMap<String, Volume> {
        val out = mutableMapOf<String, Volume>()
        runCatching {
            val o = JSONObject(store.getText(STATE_KEY) ?: return out)
            for (k in o.keys()) {
                val v = o.getJSONObject(k)
                out[k] = Volume(v.getString("version"), v.getLong("first"), v.getLong("generation"), v.getLong("cursor"))
            }
        }
        return out
    }

    private fun saveState(store: Store, state: Map<String, Volume>) {
        val o = JSONObject()
        for ((k, v) in state) {
            o.put(k, JSONObject().put("version", v.version).put("first", v.first).put("generation", v.generation).put("cursor", v.cursor))
        }
        store.setText(STATE_KEY, o.toString())
    }

    /**
     * The next look starts over, as when backup is turned on again or a folder is chosen: noted, and
     * done by `queueNew` as it begins, since one going on would save over a state forgotten here. Call
     * it after saving the backup it is for, so that the look which takes it up reads that one.
     */
    fun restart(store: Store) {
        store.setText(RESTART_KEY, "1")
    }

    private val projection = arrayOf(
        MediaStore.MediaColumns._ID,
        MediaStore.MediaColumns.RELATIVE_PATH,
        MediaStore.MediaColumns.DISPLAY_NAME,
        MediaStore.Files.FileColumns.MEDIA_TYPE,
        MediaStore.MediaColumns.DATE_TAKEN,
        MediaStore.MediaColumns.DATE_MODIFIED,
        MediaStore.MediaColumns.DATE_ADDED,
        MediaStore.MediaColumns.GENERATION_MODIFIED,
    )

    /**
     * Queues what is new since the last look, at most about `BATCH` items of the first pass at a
     * time: answers whether more is left. The first look after backup is turned on notes each
     * volume's generation before anything else, so that nothing added while it goes through is missed.
     */
    fun queueNew(context: Context, store: Store): Boolean {
        if (store.getText(RESTART_KEY) != null) {
            store.setText(STATE_KEY, null)
            store.setText(RESTART_KEY, null)
        }
        val backup = Backup.load(store)
        if (!backup.enabled || !authorized(context)) return false
        val state = loadState(store)
        var more = false
        for (volume in MediaStore.getExternalVolumeNames(context)) {
            val version = MediaStore.getVersion(context, volume)
            var v = state[volume]
            if (v == null || v.version != version) {
                // A volume not seen before, or whose database was made anew, so that its generations
                // start over: gone through again from its start, which queues nothing twice.
                val g = MediaStore.getGeneration(context, volume)
                v = Volume(version, g, g, 0)
                state[volume] = v
                saveState(store, state)
            }
            val uri = MediaStore.Files.getContentUri(volume)
            val base = mutableListOf("${MediaStore.Files.FileColumns.MEDIA_TYPE} IN ($KIND_IMAGE, $KIND_VIDEO)")
            val args = mutableListOf<String>()

            if (v.cursor >= 0) {
                // What was there before: everything, or what was taken from `since` on, by ID.
                val sel = base.toMutableList()
                val selArgs = args.toMutableList()
                sel += "${MediaStore.MediaColumns._ID} > ?"
                selArgs += v.cursor.toString()
                if (!backup.everything) {
                    sel += "COALESCE(NULLIF(${MediaStore.MediaColumns.DATE_TAKEN}, 0), ${MediaStore.MediaColumns.DATE_MODIFIED} * 1000) >= CAST(? AS INTEGER)"
                    selArgs += (backup.since * 1000).toString()
                }
                val (batch, last, n) = query(context.contentResolver, uri, volume, sel, selArgs, "${MediaStore.MediaColumns._ID} ASC", BATCH, backup::wants)
                Log.i(TAG, "$volume: $n there before, after ID ${v.cursor}, ${batch.size} of them chosen")
                if (batch.isNotEmpty()) store.plan(backup.destination, batch)
                v = if (n >= BATCH) {
                    more = true
                    v.copy(cursor = last.first)
                } else {
                    v.copy(cursor = -1)
                }
                state[volume] = v
                saveState(store, state)
                if (v.cursor >= 0) continue
            }

            // Then what the volume says changed since, planned in batches, the generation kept only once
            // all of it is queued; one queued already is answered as it stands. Only from now on: what
            // was added since backup began, or taken since, and not an old photo edited or moved.
            val sel = base.toMutableList()
            val selArgs = args.toMutableList()
            sel += "${MediaStore.MediaColumns.GENERATION_MODIFIED} > ?"
            selArgs += v.generation.toString()
            if (!backup.everything) {
                sel += "(${MediaStore.MediaColumns.GENERATION_ADDED} > ? OR " +
                    "COALESCE(NULLIF(${MediaStore.MediaColumns.DATE_TAKEN}, 0), ${MediaStore.MediaColumns.DATE_MODIFIED} * 1000) >= CAST(? AS INTEGER))"
                selArgs += v.first.toString()
                selArgs += (backup.since * 1000).toString()
            }
            val (batch, last, _) = query(context.contentResolver, uri, volume, sel, selArgs, "${MediaStore.MediaColumns.GENERATION_MODIFIED} ASC", 0, backup::wants)
            if (batch.isNotEmpty()) Log.i(TAG, "$volume: ${batch.size} changed after generation ${v.generation}")
            for (chunk in batch.chunked(BATCH)) store.plan(backup.destination, chunk)
            if (last.second > v.generation) {
                state[volume] = v.copy(generation = last.second)
                saveState(store, state)
            }
        }
        return more
    }

    /**
     * The resources a query finds in the folders `wants` takes, and of every row it had, the last ID,
     * the highest generation and how many.
     */
    private fun query(
        resolver: ContentResolver, uri: Uri, volume: String, sel: List<String>, args: List<String>,
        order: String, limit: Int, wants: (String) -> Boolean,
    ): Triple<List<AssetResource>, Pair<Long, Long>, Int> {
        val q = Bundle().apply {
            putString(ContentResolver.QUERY_ARG_SQL_SELECTION, sel.joinToString(" AND "))
            putStringArray(ContentResolver.QUERY_ARG_SQL_SELECTION_ARGS, args.toTypedArray())
            putString(ContentResolver.QUERY_ARG_SQL_SORT_ORDER, order)
            if (limit > 0) putInt(ContentResolver.QUERY_ARG_LIMIT, limit)
        }
        val out = mutableListOf<AssetResource>()
        var lastId = 0L
        var maxGen = 0L
        var rows = 0
        resolver.query(uri, projection, q, null)?.use { c ->
            val id = c.getColumnIndexOrThrow(MediaStore.MediaColumns._ID)
            val name = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DISPLAY_NAME)
            val type = c.getColumnIndexOrThrow(MediaStore.Files.FileColumns.MEDIA_TYPE)
            val taken = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DATE_TAKEN)
            val modified = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DATE_MODIFIED)
            val added = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DATE_ADDED)
            val gen = c.getColumnIndexOrThrow(MediaStore.MediaColumns.GENERATION_MODIFIED)
            val dir = c.getColumnIndexOrThrow(MediaStore.MediaColumns.RELATIVE_PATH)
            while (c.moveToNext()) {
                rows++
                val i = c.getLong(id)
                lastId = maxOf(lastId, i)
                maxGen = maxOf(maxGen, c.getLong(gen))
                if (!wants(c.getString(dir) ?: "")) continue
                val ms = when {
                    !c.isNull(taken) && c.getLong(taken) > 0 -> c.getLong(taken)
                    !c.isNull(modified) && c.getLong(modified) > 0 -> c.getLong(modified) * 1000
                    else -> c.getLong(added) * 1000
                }
                val sec = Math.floorDiv(ms, 1000L)
                val nsec = (Math.floorMod(ms, 1000L) * 1_000_000).toUInt()
                val offset = TimeZone.getDefault().getOffset(ms) / 1000
                out += AssetResource(
                    asset = "$volume:$i",
                    kind = c.getInt(type),
                    idx = 0u,
                    originalName = c.getString(name) ?: "photo",
                    createdSec = sec,
                    createdNsec = nsec,
                    utcOffset = offset,
                )
            }
        }
        return Triple(out, lastId to maxGen, rows)
    }

    /** The content URI of an upload's photo or video, by which its original bytes are read. */
    fun uri(u: Upload): Uri? {
        val (volume, id) = u.asset.split(':', limit = 2).takeIf { it.size == 2 } ?: return null
        val n = id.toLongOrNull() ?: return null
        val base = when (u.kind) {
            KIND_IMAGE -> MediaStore.Images.Media.getContentUri(volume)
            KIND_VIDEO -> MediaStore.Video.Media.getContentUri(volume)
            else -> return null
        }
        return ContentUris.withAppendedId(base, n)
    }

    /** The folder the item is in (MediaStore's relative path), or null once it is no longer on the phone. */
    fun folderOf(context: Context, uri: Uri): String? =
        context.contentResolver.query(uri, arrayOf(MediaStore.MediaColumns.RELATIVE_PATH), null, null, null)?.use {
            if (it.moveToFirst()) it.getString(0) ?: "" else null
        }

    /** Each folder on the phone that holds photos or videos, with how many, by its path. */
    fun folders(context: Context): List<MediaFolder> {
        if (!authorized(context)) return emptyList()
        val counts = sortedMapOf<String, Int>(String.CASE_INSENSITIVE_ORDER)
        for (volume in MediaStore.getExternalVolumeNames(context)) {
            context.contentResolver.query(
                MediaStore.Files.getContentUri(volume), arrayOf(MediaStore.MediaColumns.RELATIVE_PATH),
                "${MediaStore.Files.FileColumns.MEDIA_TYPE} IN ($KIND_IMAGE, $KIND_VIDEO)", null, null,
            )?.use { c ->
                while (c.moveToNext()) {
                    val p = c.getString(0) ?: continue
                    counts[p] = (counts[p] ?: 0) + 1
                }
            }
        }
        return counts.map { (p, n) -> MediaFolder(p, n) }
    }

    /**
     * What waits to be sent, or was not sent, from a folder no longer chosen, forgotten (Store.forget):
     * one already sent, its answer not in, goes on. Answers how many were forgotten.
     */
    fun forgetUnwanted(context: Context, store: Store, backup: Backup): Int {
        val waiting = (store.pending(UInt.MAX_VALUE) + store.failed(UInt.MAX_VALUE))
            .filter { it.state != UploadState.SENT && it.state != UploadState.DONE && !Added.isAdded(it) }
        var n = 0
        // Each volume's items looked up in MediaStore 500 at a time.
        val byVolume = waiting.groupBy { it.asset.substringBefore(':') }
        for ((volume, ups) in byVolume) {
            for (chunk in ups.chunked(BATCH)) {
                val ids = chunk.mapNotNull { it.asset.substringAfter(':').toLongOrNull() }
                val dirs = mutableMapOf<Long, String>()
                runCatching {
                    context.contentResolver.query(
                        MediaStore.Files.getContentUri(volume),
                        arrayOf(MediaStore.MediaColumns._ID, MediaStore.MediaColumns.RELATIVE_PATH),
                        "${MediaStore.MediaColumns._ID} IN (${ids.joinToString(",") { "?" }})",
                        ids.map { it.toString() }.toTypedArray(), null,
                    )?.use { c ->
                        while (c.moveToNext()) dirs[c.getLong(0)] = c.getString(1) ?: ""
                    }
                }.onFailure { Log.w(TAG, "$volume: folders of what waits not read: $it") }
                for (u in chunk) {
                    // One no longer on the phone is left as it is: sending it says so.
                    val dir = dirs[u.asset.substringAfter(':').toLongOrNull()] ?: continue
                    if (!backup.wants(dir) && store.forget(u.id)) n++
                }
            }
        }
        if (n > 0) Log.i(TAG, "$n waiting from folders no longer chosen, forgotten")
        return n
    }
}
