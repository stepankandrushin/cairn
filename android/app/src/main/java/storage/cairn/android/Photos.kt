// Backup of the phone's photos and videos: where they go and what they are called, which and when, how
// far it is, and what was not sent and why, each with Retry.

package storage.cairn.android

import android.Manifest
import android.os.Build
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
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun PhotosScreen() {
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
fun Toggle(text: String, on: Boolean, tag: String? = null, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable { onChange(!on) }.padding(horizontal = 16.dp, vertical = 6.dp)
            .let { if (tag != null) it.testTag(tag) else it },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(text, Modifier.weight(1f))
        Switch(checked = on, onCheckedChange = onChange)
    }
}

@Composable
fun BackupSetup() {
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
            Header("Which")
            Choice("The camera's (DCIM)", b.cameraOnly) { b = b.copy(cameraOnly = true) }
            Choice("Every photo and video on the phone", !b.cameraOnly) { b = b.copy(cameraOnly = false) }
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
                    enabled = !busy && b.folder.isNotEmpty() && b.path.isNotBlank(),
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BackupStatus() {
    var refreshing by remember { mutableStateOf(false) }
    val b = Model.backup
    val s = Model.stats
    PullToRefreshBox(isRefreshing = refreshing, onRefresh = { Model.refresh() }) {
        LazyColumn(Modifier.fillMaxWidth()) {
            item {
                Labeled("Folder", "${Model.title(b.folder)}/${b.path}")
                Labeled("Sent", "${s.done}", Modifier.testTag("sentCount"))
                Labeled("Waiting", "${s.planned + s.sent}", Modifier.testTag("waitingCount"))
                if (s.failed + s.skipped > 0uL) Labeled("Not sent", "${s.failed + s.skipped}")
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
