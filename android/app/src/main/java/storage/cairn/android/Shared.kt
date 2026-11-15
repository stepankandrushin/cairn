// What the app, its workers and its document provider share: the core's library, its database, the
// account signed in with its token sealed by the Android keystore, and the backup's settings.

package storage.cairn.android

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import org.json.JSONArray
import org.json.JSONObject
import uniffi.cairn_mobile.CairnException
import uniffi.cairn_mobile.Destination
import uniffi.cairn_mobile.FolderInfo
import uniffi.cairn_mobile.NameStyle
import uniffi.cairn_mobile.Session
import uniffi.cairn_mobile.Store
import uniffi.cairn_mobile.TransferProgress
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** The core's library, loaded once, and given the app's context before any request. */
object Native {
    @Volatile private var ready = false

    init {
        System.loadLibrary("cairn_mobile")
    }

    /** rustls-platform-verifier's start: Android checks the server's certificate (crates/mobile/src/android.rs). */
    @JvmStatic external fun init(context: Context)

    fun ensure(context: Context) {
        if (ready) return
        synchronized(this) {
            if (!ready) {
                init(context.applicationContext)
                ready = true
            }
        }
    }
}

object Cairn {
    @Volatile private var opened: Store? = null

    /** The database the app, its workers and its provider share, in the app's own files. */
    fun store(context: Context): Store {
        opened?.let { return it }
        synchronized(this) {
            opened?.let { return it }
            Native.ensure(context)
            val s = Store.open(File(context.filesDir, "cairn.db").path)
            opened = s
            return s
        }
    }

    /** The account's session, or why there is none. */
    fun session(context: Context): Session {
        val store = store(context)
        val a = Account.load(store) ?: throw CairnException.Refused(401.toUShort(), "not signed in")
        return a.session(store)
    }
}

/** The server signed in to, as this device. The token is apart, sealed (`Vault`). */
data class Account(
    val server: String,
    val device: String,
    val deviceName: String,
    val user: String,
    val allowHttp: Boolean,
) {
    fun save(store: Store) {
        val o = JSONObject()
            .put("server", server).put("device", device).put("deviceName", deviceName)
            .put("user", user).put("allowHTTP", allowHttp)
        store.setText(KEY, o.toString())
    }

    fun session(store: Store): Session {
        val token = Vault.token(store)
            ?: throw CairnException.Refused(401.toUShort(), "no token on this phone: sign in again")
        return Session(server, token, device, deviceName, allowHttp)
    }

    companion object {
        const val KEY = "account"

        fun load(store: Store): Account? = runCatching {
            val o = JSONObject(store.getText(KEY) ?: return null)
            Account(
                o.getString("server"), o.getString("device"), o.getString("deviceName"),
                o.getString("user"), o.optBoolean("allowHTTP", false),
            )
        }.getOrNull()
    }
}

/**
 * The token, sealed by a key the Android keystore keeps and never lets out: the database holds only
 * what that key sealed, so a copy of the app's files does not give the token.
 */
object Vault {
    private const val ALIAS = "storage.cairn.token"
    private const val KEY = "token.sealed"

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as SecretKey?)?.let { return it }
        val g = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        g.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build(),
        )
        return g.generateKey()
    }

    fun save(store: Store, token: String) {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        val sealed = c.doFinal(token.toByteArray())
        store.set(KEY, byteArrayOf(c.iv.size.toByte()) + c.iv + sealed)
    }

    fun token(store: Store): String? {
        val b = store.get(KEY) ?: return null
        return runCatching {
            val n = b[0].toInt()
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, b, 1, n))
            String(c.doFinal(b, 1 + n, b.size - 1 - n))
        }.getOrNull()
    }

    fun delete(store: Store) {
        store.set(KEY, null)
        runCatching { KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(ALIAS) }
    }
}

/** Where photos go, what they are called, which are sent and when, and whether backup is on. */
data class Backup(
    val enabled: Boolean = false,
    val folder: String = "",
    val path: String = "Photos",
    /** As the camera named them, `20240512_114915.jpg`, as Nextcloud's Android app kept them. */
    val originalNames: Boolean = true,
    val subfolders: Boolean = false,
    /** Everything already on the phone too, or only what is taken from `since` on. */
    val everything: Boolean = false,
    val since: Long = System.currentTimeMillis() / 1000,
    /** Only the camera's (DCIM), or every photo and video on the phone. */
    val cameraOnly: Boolean = true,
    /** In the background, only on a network that is not metered. */
    val wifiOnly: Boolean = true,
    /** In the background, only while charging. */
    val chargingOnly: Boolean = false,
) {
    val destination: Destination
        get() = Destination(folder, path, if (originalNames) NameStyle.ORIGINAL else NameStyle.NEXTCLOUD, subfolders)

    fun save(store: Store) {
        val o = JSONObject()
            .put("enabled", enabled).put("folder", folder).put("path", path)
            .put("originalNames", originalNames).put("subfolders", subfolders)
            .put("everything", everything).put("since", since).put("cameraOnly", cameraOnly)
            .put("wifiOnly", wifiOnly).put("chargingOnly", chargingOnly)
        store.setText(KEY, o.toString())
    }

    companion object {
        const val KEY = "backup"

        fun load(store: Store): Backup = runCatching {
            val o = JSONObject(store.getText(KEY) ?: return Backup())
            val d = Backup()
            Backup(
                o.optBoolean("enabled", d.enabled), o.optString("folder", d.folder),
                o.optString("path", d.path), o.optBoolean("originalNames", d.originalNames),
                o.optBoolean("subfolders", d.subfolders), o.optBoolean("everything", d.everything),
                o.optLong("since", d.since), o.optBoolean("cameraOnly", d.cameraOnly),
                o.optBoolean("wifiOnly", d.wifiOnly), o.optBoolean("chargingOnly", d.chargingOnly),
            )
        }.getOrDefault(Backup())
    }
}

/** The folders the token reaches, as last asked: their names and access, for the provider offline. */
object Folders {
    private const val KEY = "folders"

    fun save(store: Store, folders: List<FolderInfo>) {
        val a = JSONArray()
        for (f in folders) {
            a.put(JSONObject().put("folder", f.folder).put("name", f.name ?: JSONObject.NULL).put("access", f.access))
        }
        store.setText(KEY, a.toString())
    }

    fun load(store: Store): List<FolderInfo> = runCatching {
        val a = JSONArray(store.getText(KEY) ?: return emptyList())
        (0 until a.length()).map {
            val o = a.getJSONObject(it)
            FolderInfo(o.getString("folder"), if (o.isNull("name")) null else o.getString("name"), o.getString("access"))
        }
    }.getOrDefault(emptyList())
}

/** The folders shown in the system's Files picker, each a root of the document provider. */
object FilesFolders {
    private const val KEY = "files.folders"

    fun load(store: Store): List<String> = runCatching {
        val a = JSONArray(store.getText(KEY) ?: return emptyList())
        (0 until a.length()).map { a.getString(it) }
    }.getOrDefault(emptyList())

    fun save(store: Store, folders: List<String>) {
        store.setText(KEY, JSONArray(folders).toString())
    }
}

/** An error in words: the core's own reason, or the system's. */
fun describe(e: Throwable): String = when (e) {
    is CairnException.Refused -> e.reason
    is CairnException.NotFound -> e.reason
    is CairnException.Unreachable -> e.reason
    is CairnException.Failed -> e.reason
    is CairnException.Cancelled -> "stopped"
    else -> e.message ?: e.toString()
}

/** A transfer's progress, told to `onProgress`; it stops when `stopped` says so. */
class Watch(
    private val stopped: () -> Boolean = { false },
    private val onProgress: (Long, Long) -> Unit = { _, _ -> },
) : TransferProgress {
    override fun progress(done: ULong, total: ULong): Boolean {
        onProgress(done.toLong(), total.toLong())
        return !stopped()
    }
}
