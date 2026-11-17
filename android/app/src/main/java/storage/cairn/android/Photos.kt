// Backup of the phone's photos and videos: where they go and what they are called, which (the camera's,
// screenshots, other apps' folders, each a switch) and when, how far it is, whether the phone lets it run
// in the background, and what was not sent and why, each with Retry.

package storage.cairn.android

import android.Manifest
import android.content.Context
import android.os.Build
import android.text.format.DateFormat
import android.text.format.DateUtils
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.selection.selectable
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun PhotosScreen() {
    LaunchedEffect(Unit) { Model.loadMediaFolders() }
    Screen("Photos") {
        if (Model.backup.enabled) BackupStatus() else BackupSetup()
    }
}

@Composable
fun Choice(text: String, selected: Boolean, tag: String? = null, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().selectable(selected = selected, onClick = onClick).padding(horizontal = 16.dp, vertical = 6.dp)
            .let { if (tag != null) it.testTag(tag) else it },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        RadioButton(selected = selected, onClick = null)
        Spacer(Modifier.width(12.dp))
        Text(text)
    }
}

@Composable
fun Toggle(text: String, on: Boolean, tag: String? = null, detail: String? = null, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable { onChange(!on) }.padding(horizontal = 16.dp, vertical = 6.dp)
            .let { if (tag != null) it.testTag(tag) else it },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(text)
            if (detail != null) {
                Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
        Spacer(Modifier.width(12.dp))
        Switch(checked = on, onCheckedChange = onChange)
    }
}

/**
 * Which photos and videos, each kind a switch: the camera's, on until turned off; screenshots and
 * screen recordings, and each other app's folder, off until turned on. The folders are the phone's
 * own, so they show once Cairn may see its photos.
 */
@Composable
fun Which(b: Backup, onChange: (Backup) -> Unit) {
    val context = LocalContext.current
    val see = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { Model.loadMediaFolders() }
    val allowed = MediaLibrary.authorized(context)
    val found = Model.mediaFolders
    fun count(of: List<MediaFolder>) = of.sumOf { it.count }
    val camera = found.filter { it.kind == MediaFolder.Kind.CAMERA }
    val screens = found.filter { it.kind == MediaFolder.Kind.SCREENSHOTS }
    val others = found.filter { it.kind == MediaFolder.Kind.OTHER }.sortedBy { it.title.lowercase() }
    Header("Which")
    Toggle("Camera", b.camera, "camera", if (allowed) "DCIM · ${count(camera)}" else "DCIM") { onChange(b.copy(camera = it)) }
    Toggle(
        "Screenshots and screen recordings", b.screenshots, "screenshots",
        when {
            screens.isNotEmpty() -> screens.joinToString(", ") { it.path.trimEnd('/') } + " · ${count(screens)}"
            allowed -> "None on the phone yet"
            else -> null
        },
    ) { onChange(b.copy(screenshots = it)) }
    for (f in others) {
        Toggle(f.title, f.path in b.folders, detail = "${f.path.trimEnd('/')} · ${f.count}") { on ->
            onChange(b.copy(folders = if (on) b.folders + f.path else b.folders - f.path))
        }
    }
    // One chosen that holds nothing now stays, so that it can be turned off.
    for (p in b.folders.filter { p -> others.none { it.path == p } }) {
        Toggle(MediaFolder(p, 0).title, true, detail = "${p.trimEnd('/')} · none now") { onChange(b.copy(folders = b.folders - p)) }
    }
    when {
        !allowed -> {
            TextButton(onClick = { see.launch(MediaLibrary.permissions) }, modifier = Modifier.padding(horizontal = 8.dp)) {
                Text("Choose other apps' folders")
            }
            Footnote("WhatsApp's, Telegram's and other apps' folders of photos and videos show here once Cairn may see them.")
        }
        others.isEmpty() -> Footnote("No other app keeps photos or videos on the phone yet: the folders apps make later show here, off.")
        else -> Footnote("Other apps' folders, off until turned on: those apps make later show here too.")
    }
}

@Composable
fun BackupSetup() {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var b by remember { mutableStateOf(Model.backup) }
    var busy by remember { mutableStateOf(false) }
    var picking by remember { mutableStateOf(false) }
    val writable = Model.folders.filter { it.access != "read" }
    LaunchedEffect(Model.backup) { b = Model.backup }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        scope.launch {
            busy = true
            try {
                Model.startBackup(b)
                if (!HeldBack.exempt(context)) HeldBack.askExempt(context)
            } catch (e: Exception) {
                Model.message = describe(e)
            } finally {
                busy = false
            }
        }
    }
    LazyColumn(Modifier.fillMaxWidth()) {
        item {
            Header("Where they go")
            Box {
                Labeled("Folder", (if (b.folder.isEmpty()) "Choose" else Model.title(b.folder)) + " ▾", Modifier.clickable { picking = true }.testTag("backupFolder"))
                DropdownMenu(expanded = picking, onDismissRequest = { picking = false }) {
                    for (f in writable) {
                        DropdownMenuItem(text = { Text(f.name ?: f.folder) }, onClick = { b = b.copy(folder = f.folder); picking = false })
                    }
                }
            }
            OutlinedTextField(
                b.path, { b = b.copy(path = it) }, label = { Text("Path in it") }, singleLine = true,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp).testTag("backupPath"),
            )
            Header("Names")
            Choice("As on the phone (20240512_114915.jpg)", b.originalNames) { b = b.copy(originalNames = true) }
            Choice("Date and time first, as Nextcloud's iPhone app named them", !b.originalNames) { b = b.copy(originalNames = false) }
            Toggle("A folder for each year and month", b.subfolders) { b = b.copy(subfolders = it) }
            Footnote(example(b))
            Which(b) { b = it }
            HorizontalDivider(Modifier.padding(vertical = 4.dp, horizontal = 16.dp))
            Choice("New photos and videos, from now on", !b.everything, "fromNow") { b = b.copy(everything = false) }
            Choice("Those already on the phone too", b.everything, "everything") { b = b.copy(everything = true) }
            Footnote("Originals are sent as they are, where they were taken included. A photo deleted from the phone stays on the server.")
            Header("When")
            Toggle("Only on Wi-Fi", b.wifiOnly, "wifiOnly") { b = b.copy(wifiOnly = it) }
            Toggle("Only while charging", b.chargingOnly) { b = b.copy(chargingOnly = it) }
            Footnote("In the background. Upload now sends at once, on any network.")
            Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
                Button(
                    onClick = {
                        val perms = MediaLibrary.permissions.toMutableList()
                        if (Build.VERSION.SDK_INT >= 33) perms += Manifest.permission.POST_NOTIFICATIONS
                        ask.launch(perms.toTypedArray())
                    },
                    enabled = !busy && b.folder.isNotEmpty() && b.path.isNotBlank() && b.choosesAny,
                    modifier = Modifier.testTag("startBackup"),
                ) { Text("Turn on backup") }
                if (busy) {
                    Spacer(Modifier.width(16.dp))
                    CircularProgressIndicator(Modifier.size(24.dp))
                }
            }
        }
    }
}

private fun example(b: Backup): String {
    val now = Date()
    val name = if (b.originalNames) {
        "IMG_" + SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(now) + ".jpg"
    } else {
        // The core's Nextcloud style keeps the four characters after a camera's `IMG_`.
        SimpleDateFormat("yy-MM-dd HH-mm-ss yyyy", Locale.US).format(now) + ".jpg"
    }
    val parts = mutableListOf(b.path.trim('/'))
    if (b.subfolders) parts += SimpleDateFormat("yyyy/MM", Locale.US).format(now)
    parts += name
    return "For example: " + parts.filter { it.isNotEmpty() }.joinToString("/")
}

/**
 * When a run last started in the background; and, while the phone holds the app back there, what it
 * would allow: Android's battery optimization left off, and the setting of its maker's own.
 */
@Composable
fun InTheBackground() {
    val context = LocalContext.current
    val since = Model.heldSince
    Model.lastBegan?.let { Labeled("Last background run", at(context, it), Modifier.testTag("lastBackgroundRun")) }
    if (since != null) {
        Text(
            "This phone has not let Cairn run in the background since ${at(context, since)}, so new photos wait until the app is opened.",
            color = MaterialTheme.colorScheme.error,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp).testTag("heldBack"),
        )
    }
    if (!Model.exempt) {
        Footnote("To save battery, Android puts off what an app does in the background, for hours once it has not been opened for a while.")
        TextButton(onClick = { HeldBack.askExempt(context) }, modifier = Modifier.padding(horizontal = 8.dp).testTag("allowBackground")) {
            Text("Allow in the background")
        }
    }
    if (since != null) {
        val screen = remember { HeldBack.screen(context) }
        Footnote(screen.hint)
        OutlinedButton(onClick = { screen.open(context) }, modifier = Modifier.padding(horizontal = 16.dp).testTag("openHeldBack")) {
            Text("Open ${screen.name}")
        }
    }
}

/** A time today, or a day and time before. */
private fun at(context: Context, ms: Long): String {
    val time = DateFormat.getTimeFormat(context).format(Date(ms))
    if (DateUtils.isToday(ms)) return time
    return DateUtils.formatDateTime(context, ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH) + ", " + time
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BackupStatus() {
    var refreshing by remember { mutableStateOf(false) }
    val b = Model.backup
    val s = Model.stats
    PullToRefreshBox(isRefreshing = refreshing, onRefresh = { Model.refresh(); Model.loadMediaFolders() }) {
        LazyColumn(Modifier.fillMaxWidth()) {
            item {
                Labeled("Folder", "${Model.title(b.folder)}/${b.path}")
                Labeled("Sent", "${s.done}", Modifier.testTag("sentCount"))
                Labeled("Waiting", "${s.planned + s.sent}", Modifier.testTag("waitingCount"))
                if (s.failed + s.skipped > 0uL) Labeled("Not sent", "${s.failed + s.skipped}")
                InTheBackground()
                Model.uploadNote?.let { Footnote(it) }
                Row(Modifier.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                    Button(onClick = { Model.uploadNow() }, modifier = Modifier.testTag("uploadNow")) { Text("Upload now") }
                    if (Model.uploading) {
                        Spacer(Modifier.width(16.dp))
                        CircularProgressIndicator(Modifier.size(24.dp))
                    }
                }
                Footnote(
                    "New photos are sent a little after they are taken, and what waits every hour, " +
                        (if (b.wifiOnly) "on Wi-Fi" else "on any network") + (if (b.chargingOnly) " while charging" else "") +
                        "; Upload now sends what waits at once.",
                )
                Header("When")
                Toggle("Only on Wi-Fi", b.wifiOnly) { Model.saveBackup(b.copy(wifiOnly = it)) }
                Toggle("Only while charging", b.chargingOnly) { Model.saveBackup(b.copy(chargingOnly = it)) }
                Which(b) { Model.saveWhich(it) }
                Footnote(
                    (if (b.everything) "Turned on, a folder's photos and videos already on the phone are sent too"
                    else "Turned on, a folder's photos and videos taken since backup was turned on are sent") +
                        "; turned off, what waits from it is not. What was sent stays on the server." +
                        (if (b.choosesAny) "" else " Nothing is chosen, so nothing new is sent."),
                )
            }
            if (Model.failed.isNotEmpty()) {
                item { Header("Not sent") }
                items(Model.failed, key = { it.id }) { u ->
                    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp)) {
                        Text(u.originalName)
                        Text(u.error ?: "", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
                        TextButton(onClick = { Model.retry(u) }, contentPadding = ButtonDefaults.TextButtonContentPadding) { Text("Retry") }
                    }
                }
                item { TextButton(onClick = { Model.retryAll() }, modifier = Modifier.padding(horizontal = 8.dp)) { Text("Retry all") } }
            }
            if (Model.recent.isNotEmpty()) {
                item { Header("Last sent") }
                items(Model.recent, key = { it.id }) { u ->
                    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp)) {
                        Text(u.name ?: u.path)
                        if (u.name != null && !u.path.endsWith(u.name!!)) {
                            Footnote("Its name was taken by other bytes", inset = false)
                        }
                    }
                }
            }
            item {
                Column(Modifier.padding(vertical = 16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    OutlinedButton(
                        onClick = { Model.stopBackup() },
                        modifier = Modifier.padding(horizontal = 16.dp),
                        colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
                    ) { Text("Turn off backup") }
                    Footnote("What was sent stays on the server.")
                }
            }
        }
    }
}
