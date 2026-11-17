package storage.cairn.android

import android.app.Application
import android.content.ClipboardManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch

class CairnApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Native.ensure(this)
        Background.channel(this)
        Model.init(this)
        Background.schedule(this)
    }
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        Model.init(this)
        setContent {
            CairnTheme {
                // The test tags as resource IDs, for UI Automator.
                @OptIn(ExperimentalComposeUiApi::class)
                Box(Modifier.semantics { testTagsAsResourceId = true }) { Root() }
            }
        }
    }

    override fun onStart() {
        super.onStart()
        Model.shown()
    }

    override fun onResume() {
        super.onResume()
        Model.opened()
    }

    override fun onStop() {
        super.onStop()
        Model.left()
    }
}

@Composable
fun CairnTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    val ctx = LocalContext.current
    val scheme = when {
        Build.VERSION.SDK_INT >= 31 -> if (dark) dynamicDarkColorScheme(ctx) else dynamicLightColorScheme(ctx)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = scheme, content = content)
}

@Composable
fun Root() {
    if (Model.account == null) SignInScreen() else MainScreen()
    Model.message?.let { m ->
        AlertDialog(
            onDismissRequest = { Model.message = null },
            confirmButton = { TextButton(onClick = { Model.message = null }) { Text("OK") } },
            title = { Text("Cairn") },
            text = { Text(m) },
        )
    }
}

/** A tab's or a folder's screen: its bar, and what is under it. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun Screen(
    title: String,
    back: (() -> Unit)? = null,
    actions: @Composable RowScope.() -> Unit = {},
    content: @Composable () -> Unit,
) {
    Scaffold(
        contentWindowInsets = WindowInsets(0, 0, 0, 0),
        topBar = {
            TopAppBar(
                title = { Text(title, maxLines = 1) },
                navigationIcon = {
                    if (back != null) {
                        TextButton(onClick = back) { Icon(painterResource(R.drawable.ic_back), "Back") }
                    }
                },
                actions = actions,
            )
        },
    ) { padding ->
        Box(Modifier.padding(padding).fillMaxSize()) { content() }
    }
}

/** A note under what it is about; `inset` false where the column has its own margin. */
@Composable
fun Footnote(text: String, inset: Boolean = true) {
    Text(
        text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(horizontal = if (inset) 16.dp else 0.dp, vertical = 4.dp),
    )
}

@Composable
fun Header(text: String) {
    Text(
        text, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 20.dp, bottom = 4.dp),
    )
}

@Composable
fun MainScreen() {
    var tab by rememberSaveable { mutableIntStateOf(0) }
    LaunchedEffect(Unit) { Model.loadFolders() }
    Scaffold(
        contentWindowInsets = WindowInsets(0, 0, 0, 0),
        bottomBar = {
            NavigationBar {
                val items = listOf("Photos" to R.drawable.ic_photos, "Browse" to R.drawable.ic_folder, "Settings" to R.drawable.ic_settings)
                items.forEachIndexed { i, (label, icon) ->
                    NavigationBarItem(
                        selected = tab == i, onClick = { tab = i },
                        icon = { Icon(painterResource(icon), null) }, label = { Text(label) },
                        modifier = Modifier.testTag("tab-$label"),
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding).fillMaxSize()) {
            when (tab) {
                0 -> PhotosScreen()
                1 -> BrowseScreen()
                else -> SettingsScreen()
            }
        }
    }
}

@Composable
fun SignInScreen() {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var server by rememberSaveable { mutableStateOf("https://") }
    var token by rememberSaveable { mutableStateOf("") }
    var deviceName by rememberSaveable { mutableStateOf(Build.MODEL ?: "Android") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    Screen("Cairn") {
        Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("Your server", style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary)
            OutlinedTextField(
                server, { server = it }, label = { Text("Server") }, singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, autoCorrectEnabled = false),
                modifier = Modifier.fillMaxWidth().testTag("server"),
            )
            OutlinedTextField(
                token, { token = it }, label = { Text("Token") }, singleLine = true,
                visualTransformation = PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false),
                modifier = Modifier.fillMaxWidth().testTag("token"),
            )
            TextButton(onClick = {
                val clip = ctx.getSystemService(ClipboardManager::class.java).primaryClip
                clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(ctx)?.toString()?.let { token = it.trim() }
            }) { Text("Paste token") }
            Footnote("A token is made on the server, one per device: cairn-server token add <user> --label android", inset = false)
            Spacer(Modifier.size(8.dp))
            OutlinedTextField(
                deviceName, { deviceName = it }, label = { Text("This device's name") }, singleLine = true,
                modifier = Modifier.fillMaxWidth().testTag("deviceName"),
            )
            Footnote("Shown in each file's history, and the name of its photos' folder.", inset = false)
            Spacer(Modifier.size(8.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Button(
                    onClick = {
                        scope.launch {
                            busy = true
                            try {
                                Model.signIn(server, token, deviceName)
                                error = null
                            } catch (e: Exception) {
                                error = describe(e)
                            } finally {
                                busy = false
                            }
                        }
                    },
                    enabled = !busy && token.isNotBlank() && deviceName.isNotBlank(),
                    modifier = Modifier.testTag("signIn"),
                ) { Text("Sign in") }
                if (busy) {
                    Spacer(Modifier.width(16.dp))
                    CircularProgressIndicator(Modifier.size(24.dp))
                }
            }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.testTag("signInError")) }
        }
    }
}

/** A label and its value, in a row. */
@Composable
fun Labeled(label: String, value: String, modifier: Modifier = Modifier) {
    Row(modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp), horizontalArrangement = Arrangement.SpaceBetween) {
        Text(label)
        Spacer(Modifier.width(16.dp))
        Text(value, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}
