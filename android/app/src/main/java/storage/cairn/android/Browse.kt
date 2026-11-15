// The server's folders, live: each listed when opened, a photo or video shown by its thumbnail, and a
// file downloaded only when it is opened, into a cache the system may empty; a video streams.

package storage.cairn.android

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import android.text.format.DateUtils
import android.text.format.Formatter
import android.webkit.MimeTypeMap
import android.widget.MediaController
import android.widget.VideoView
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.cairn_mobile.CairnException
import uniffi.cairn_mobile.Entry
import uniffi.cairn_mobile.EntryKind
import uniffi.cairn_mobile.Listing
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean

data class Place(val folder: String, val dir: String, val title: String)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BrowseScreen() {
    var stack by remember { mutableStateOf(listOf<Place>()) }
    var refreshing by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    BackHandler(enabled = stack.isNotEmpty()) { stack = stack.dropLast(1) }
    val top = stack.lastOrNull()
    if (top != null) {
        FolderScreen(top, open = { stack = stack + it }, back = { stack = stack.dropLast(1) })
        return
    }
    Screen("Browse") {
        PullToRefreshBox(isRefreshing = refreshing, onRefresh = {
            scope.launch {
                refreshing = true
                Model.loadFolders()
                refreshing = false
            }
        }) {
            LazyColumn(Modifier.fillMaxSize()) {
                items(Model.folders, key = { it.folder }) { f ->
                    ListItem(
                        headlineContent = { Text(f.name ?: f.folder) },
                        supportingContent = if (f.access == "read") ({ Text("read only") }) else null,
                        leadingContent = { Icon(painterResource(R.drawable.ic_folder), null) },
                        modifier = Modifier.clickable { stack = stack + Place(f.folder, "root", f.name ?: f.folder) }
                            .testTag("folder-${f.folder}"),
                    )
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun FolderScreen(place: Place, open: (Place) -> Unit, back: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var listing by remember(place) { mutableStateOf<Listing?>(null) }
    var error by remember(place) { mutableStateOf<String?>(null) }
    var grid by remember(place) { mutableStateOf<Boolean?>(null) }
    var refreshing by remember { mutableStateOf(false) }
    var opening by remember { mutableStateOf<Entry?>(null) }

    suspend fun load() {
        try {
            listing = withContext(Dispatchers.IO) { OpenFolders.get(ctx, place.folder).list(place.dir) }
            error = null
        } catch (e: Exception) {
            error = describe(e)
        }
    }
    LaunchedEffect(place) { load() }

    val entries = listing?.entries.orEmpty()
    val showGrid = grid ?: (entries.count { Media.isMedia(it) } * 2 > entries.size)
    Screen(place.title, back = back, actions = {
        IconButton(onClick = { grid = !showGrid }) {
            Icon(painterResource(if (showGrid) R.drawable.ic_list else R.drawable.ic_grid), if (showGrid) "List" else "Grid")
        }
    }) {
        PullToRefreshBox(isRefreshing = refreshing, onRefresh = {
            scope.launch {
                refreshing = true
                load()
                refreshing = false
            }
        }) {
            when {
                listing != null && showGrid -> LazyVerticalGrid(
                    GridCells.Adaptive(110.dp), Modifier.fillMaxSize(),
                    horizontalArrangement = Arrangement.spacedBy(2.dp), verticalArrangement = Arrangement.spacedBy(2.dp),
                ) {
                    items(entries, key = { it.`object` }) { e ->
                        Cell(place.folder, e, Modifier.clickable {
                            when (e.kind) {
                                EntryKind.DIR -> open(Place(place.folder, e.`object`, e.display))
                                EntryKind.FILE -> opening = e
                                EntryKind.SYMLINK -> {}
                            }
                        }.testTag("entry-${e.display}"))
                    }
                }
                listing != null -> LazyColumn(Modifier.fillMaxSize()) {
                    items(entries, key = { it.`object` }) { e -> EntryRow(place, e, open = open, opening = { opening = it }) }
                }
                error != null -> Column(Modifier.fillMaxSize().padding(32.dp), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
                    Text("Not listed", style = MaterialTheme.typography.titleMedium)
                    Text(error ?: "", textAlign = TextAlign.Center, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                else -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
            }
        }
    }
    opening?.let { e -> Preview(place.folder, e) { opening = null } }
}

@Composable
private fun Cell(folder: String, e: Entry, modifier: Modifier) {
    if (e.kind == EntryKind.FILE && Media.isMedia(e)) {
        Thumb(folder, e, modifier.aspectRatio(1f))
        return
    }
    Column(
        modifier.aspectRatio(1f).background(MaterialTheme.colorScheme.surfaceVariant).padding(6.dp),
        verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Icon(painterResource(icon(e)), null, Modifier.size(40.dp))
        Text(e.display, style = MaterialTheme.typography.bodySmall, maxLines = 2, overflow = TextOverflow.Ellipsis, textAlign = TextAlign.Center)
    }
}

private fun icon(e: Entry) = when {
    e.kind == EntryKind.DIR -> R.drawable.ic_folder
    e.kind == EntryKind.SYMLINK -> R.drawable.ic_link
    Media.isVideo(e) -> R.drawable.ic_movie
    Media.isImage(e) -> R.drawable.ic_image
    else -> R.drawable.ic_file
}

@Composable
private fun EntryRow(place: Place, e: Entry, open: (Place) -> Unit, opening: (Entry) -> Unit) {
    val ctx = LocalContext.current
    ListItem(
        headlineContent = { Text(e.display) },
        supportingContent = if (e.kind == EntryKind.DIR) null else ({ Text(detail(ctx, e)) }),
        leadingContent = {
            if (Media.isMedia(e)) Thumb(place.folder, e, Modifier.size(44.dp)) else Icon(painterResource(icon(e)), null, Modifier.size(32.dp))
        },
        modifier = Modifier.clickable(enabled = e.kind != EntryKind.SYMLINK) {
            if (e.kind == EntryKind.DIR) open(Place(place.folder, e.`object`, e.display)) else opening(e)
        }.testTag("entry-${e.display}"),
    )
}

private fun detail(ctx: Context, e: Entry): String {
    if (e.kind == EntryKind.SYMLINK) return "link to ${e.target ?: ""}"
    val size = Formatter.formatShortFileSize(ctx, (e.size ?: 0uL).toLong())
    val date = DateUtils.formatDateTime(ctx, e.mtimeSec * 1000, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_SHOW_TIME or DateUtils.FORMAT_ABBREV_MONTH)
    return "$size, $date"
}

@Composable
fun Thumb(folder: String, e: Entry, modifier: Modifier = Modifier) {
    val ctx = LocalContext.current
    val image by produceState<Bitmap?>(null, e.revision) { value = Thumbnails.image(ctx, folder, e) }
    Box(modifier.background(MaterialTheme.colorScheme.surfaceVariant), contentAlignment = Alignment.Center) {
        val i = image
        if (i != null) {
            Image(i.asImageBitmap(), null, contentScale = ContentScale.Crop, modifier = Modifier.fillMaxSize())
        } else {
            Icon(painterResource(icon(e)), null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        if (Media.isVideo(e)) {
            Icon(
                painterResource(R.drawable.ic_play), null, tint = Color.White,
                modifier = Modifier.align(Alignment.BottomEnd).padding(4.dp).size(20.dp),
            )
        }
    }
}

/** A file opened: a video streamed, a photo downloaded and shown, anything else downloaded and handed to an app. */
@Composable
fun Preview(folder: String, e: Entry, close: () -> Unit) {
    val ctx = LocalContext.current
    var file by remember { mutableStateOf<File?>(null) }
    var image by remember { mutableStateOf<Bitmap?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var fraction by remember { mutableFloatStateOf(0f) }
    val cancelled = remember { AtomicBoolean(false) }
    val video = Media.isVideo(e)

    LaunchedEffect(e.`object`) {
        if (video) return@LaunchedEffect
        try {
            val f = withContext(Dispatchers.IO) {
                Cache.file(ctx, OpenFolders.get(ctx, folder), e.`object`, e.revision, e.display, e.size ?: 0uL, { cancelled.get() }) { done, total ->
                    if (total > 0) fraction = done.toFloat() / total
                }
            }
            if (Media.isImage(e)) image = withContext(Dispatchers.IO) { decode(f) }
            file = f
            if (image == null) openWith(ctx, f, e.display)
        } catch (_: CairnException.Cancelled) {
        } catch (x: Exception) {
            error = describe(x)
        }
    }

    Dialog(onDismissRequest = { cancelled.set(true); close() }, properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false)) {
        Box(Modifier.fillMaxSize().background(Color.Black)) {
            when {
                video -> VideoPlayer(folder, e)
                image != null -> Zoomable(image!!)
                error != null -> Column(Modifier.fillMaxSize().padding(32.dp), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
                    Text("Not opened", color = Color.White, style = MaterialTheme.typography.titleMedium)
                    Text(error ?: "", color = Color.LightGray, textAlign = TextAlign.Center)
                }
                file != null -> Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
                    Icon(painterResource(R.drawable.ic_file), null, tint = Color.White, modifier = Modifier.size(64.dp))
                    Spacer(Modifier.size(16.dp))
                    Button(onClick = { openWith(ctx, file!!, e.display) }) { Text("Open") }
                }
                else -> Column(Modifier.fillMaxSize().padding(32.dp), verticalArrangement = Arrangement.Center) {
                    LinearProgressIndicator(progress = { fraction }, modifier = Modifier.fillMaxWidth())
                }
            }
            Row(Modifier.fillMaxWidth().statusBarsPadding().background(Color(0x66000000)).padding(4.dp), verticalAlignment = Alignment.CenterVertically) {
                IconButton(onClick = { cancelled.set(true); close() }) { Icon(painterResource(R.drawable.ic_close), "Done", tint = Color.White) }
                Text(e.display, color = Color.White, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                file?.let { f ->
                    IconButton(onClick = { share(ctx, f, e.display) }) { Icon(painterResource(R.drawable.ic_share), "Share", tint = Color.White) }
                }
            }
        }
    }
}

/** A photo made no bigger than a screen needs, turned as it was taken. */
private fun decode(f: File): Bitmap? = runCatching {
    ImageDecoder.decodeBitmap(ImageDecoder.createSource(f)) { d, info, _ ->
        val side = maxOf(info.size.width, info.size.height)
        if (side > 4096) {
            val s = 4096f / side
            d.setTargetSize((info.size.width * s).toInt(), (info.size.height * s).toInt())
        }
    }
}.getOrNull()

@Composable
private fun Zoomable(b: Bitmap) {
    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    Image(
        b.asImageBitmap(), null, contentScale = ContentScale.Fit,
        modifier = Modifier.fillMaxSize().testTag("photo")
            .pointerInput(Unit) {
                detectTransformGestures { _, pan, zoom, _ ->
                    scale = (scale * zoom).coerceIn(1f, 8f)
                    offset = if (scale == 1f) Offset.Zero else offset + pan
                }
            }
            .pointerInput(Unit) { detectTapGestures(onDoubleTap = { scale = 1f; offset = Offset.Zero }) }
            .graphicsLayer(scaleX = scale, scaleY = scale, translationX = offset.x, translationY = offset.y),
    )
}

/** A video streamed by the platform's player, which sends the token in its requests. */
@Composable
private fun VideoPlayer(folder: String, e: Entry) {
    val ctx = LocalContext.current
    val req by produceState<uniffi.cairn_mobile.HttpRequest?>(null, e.revision) {
        value = withContext(Dispatchers.IO) { runCatching { OpenFolders.get(ctx, folder).contentRequest(e.`object`, e.revision) }.getOrNull() }
    }
    val r = req ?: return Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
    AndroidView(
        modifier = Modifier.fillMaxSize(),
        factory = { c ->
            VideoView(c).apply {
                val controls = MediaController(c)
                controls.setAnchorView(this)
                setMediaController(controls)
                setVideoURI(Uri.parse(r.url), r.headers.associate { it.name to it.value })
                setOnPreparedListener { start() }
            }
        },
        onRelease = { it.stopPlayback() },
    )
}

private fun mime(name: String) =
    MimeTypeMap.getSingleton().getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase()) ?: "application/octet-stream"

private fun uri(ctx: Context, f: File) = FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", f)

private fun openWith(ctx: Context, f: File, name: String) {
    val i = Intent(Intent.ACTION_VIEW).setDataAndType(uri(ctx, f), mime(name)).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    try {
        ctx.startActivity(Intent.createChooser(i, name))
    } catch (_: ActivityNotFoundException) {
        Model.message = "No app on this phone opens $name"
    }
}

private fun share(ctx: Context, f: File, name: String) {
    val i = Intent(Intent.ACTION_SEND).setType(mime(name)).putExtra(Intent.EXTRA_STREAM, uri(ctx, f))
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    ctx.startActivity(Intent.createChooser(i, name))
}
