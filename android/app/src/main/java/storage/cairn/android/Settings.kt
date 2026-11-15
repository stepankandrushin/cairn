// The account, the folders shown in the Files picker, what it could not be shown and why, the cache,
// and signing out.

package storage.cairn.android

import android.text.format.Formatter
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.cairn_mobile.coreVersion

@Composable
fun SettingsScreen() {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var cache by remember { mutableLongStateOf(0L) }
    var confirm by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        cache = withContext(Dispatchers.IO) { Cache.size(ctx) }
        Model.refresh()
    }
    Screen("Settings") {
        LazyColumn(Modifier.fillMaxSize()) {
            Model.account?.let { a ->
                item {
                    Header("Account")
                    Labeled("Server", a.server)
                    Labeled("User", a.user)
                    Labeled("This device", a.deviceName)
                    Labeled("Version", "${BuildConfig.VERSION_NAME} (core ${coreVersion()})")
                }
            }
            item { Header("In the Files picker") }
            items(Model.folders, key = { it.folder }) { f ->
                Toggle(f.name ?: f.folder, f.folder in Model.filesFolders, "files-${f.folder}") { on -> Model.showInFiles(f.folder, on) }
            }
            item {
                Footnote(
                    "Each is a place in the system's Files picker, where a file is downloaded when it is opened and kept only while the " +
                        "system has room. New files and folders can be added there, and from any app that saves through it; changing " +
                        "what is on the server is done on a computer.",
                )
            }
            if (Model.skipped.isNotEmpty()) {
                item { Header("Not shown in Files") }
                items(Model.skipped, key = { it.folder + "/" + it.`object` }) { s ->
                    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp)) {
                        Text(s.display)
                        Text(s.reason, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }
            item {
                Header("On this phone")
                Labeled("Opened files and thumbnails", Formatter.formatShortFileSize(ctx, cache))
                TextButton(onClick = {
                    scope.launch {
                        withContext(Dispatchers.IO) { Cache.clear(ctx) }
                        cache = withContext(Dispatchers.IO) { Cache.size(ctx) }
                    }
                }, modifier = Modifier.padding(horizontal = 8.dp)) { Text("Clear") }
            }
            item {
                OutlinedButton(
                    onClick = { confirm = true },
                    modifier = Modifier.padding(start = 16.dp, top = 16.dp),
                    colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
                ) { Text("Sign out") }
                Footnote("Photos not yet sent are not sent; what is on the server stays.")
            }
        }
    }
    if (confirm) {
        AlertDialog(
            onDismissRequest = { confirm = false },
            title = { Text("Sign out?") },
            text = { Text("Photos and files not yet sent are not sent; what is on the server stays.") },
            confirmButton = { TextButton(onClick = { confirm = false; scope.launch { Model.signOut() } }) { Text("Sign out") } },
            dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel") } },
        )
    }
}
