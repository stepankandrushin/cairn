// Phones that keep the app from running in the background. Android's battery optimization puts off an
// app's jobs, for hours once it has not been opened lately, and the makers' own managers go further:
// Honor's and Huawei's App launch, Xiaomi's Autostart and the others freeze an app, drop its jobs and do
// not start it, so photos wait until it is opened, and nothing says so. The start of each run in the
// background is kept; the backup screen shows it, and says so when the phone let none start for long
// while the app was away and something waited when it came back, with the setting that allows it.

package storage.cairn.android

import android.app.ActivityManager
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings
import uniffi.cairn_mobile.Store
import java.util.concurrent.TimeUnit

object HeldBack {
    private const val BEGAN = "background.began"
    private const val LEFT = "background.left"
    private const val SINCE = "background.held"

    /** A run is due every hour: two hours without one while the app was away is the phone's doing. */
    private val LONG = TimeUnit.HOURS.toMillis(2)

    /** Whether the app is on the screen, where a run says nothing of what the phone allows in the background. */
    @Volatile private var onScreen = false

    /** This visit to the screen has been looked at; and, if the app was away long with no run, since when. */
    private var visited = false
    private var pending: Long? = null

    private fun get(store: Store, key: String): Long? = runCatching { store.getText(key)?.toLongOrNull() }.getOrNull()

    private fun set(store: Store, key: String, ms: Long?) {
        runCatching { store.setText(key, ms?.toString()) }
    }

    /** When a run last started in the background, as far as this app has kept it. */
    fun lastBegan(store: Store): Long? = get(store, BEGAN)

    /** Since when the phone has kept the app from running in the background, once seen, until a run starts there. */
    fun since(store: Store): Long? = get(store, SINCE)

    /**
     * On the screen, or about to be: a process started for the screen is in the foreground before its
     * activity starts, and WorkManager may start an overdue run in it first.
     */
    private fun foreground(): Boolean = onScreen || ActivityManager.RunningAppProcessInfo()
        .also { ActivityManager.getMyMemoryState(it) }.importance <= ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND

    /** A run of WorkManager's started: one in the background says the phone lets the app run there. */
    @Synchronized
    fun began(context: Context) {
        if (foreground()) return
        val store = Cairn.store(context)
        set(store, BEGAN, System.currentTimeMillis())
        set(store, SINCE, null)
        pending = null
    }

    /** The app is on the screen. */
    fun shown(context: Context) {
        onScreen = true
        arrived(Cairn.store(context))
    }

    /** The app left the screen. */
    @Synchronized
    fun left(context: Context) {
        onScreen = false
        visited = false
        pending = null
        set(Cairn.store(context), LEFT, System.currentTimeMillis())
    }

    /**
     * The app came to the screen: if it was away long, from the later of its leaving and the last
     * run's start in the background, with no run started since, the first look for photos says
     * whether something waited.
     */
    @Synchronized
    private fun arrived(store: Store) {
        if (visited) return
        visited = true
        val left = get(store, LEFT) ?: return
        val from = maxOf(left, get(store, BEGAN) ?: 0L)
        pending = from.takeIf { System.currentTimeMillis() - it >= LONG }
    }

    /** A run is about to look for new photos: on the screen, this visit is looked at first. */
    fun looking(context: Context) {
        if (foreground()) arrived(Cairn.store(context))
    }

    /**
     * A run looked for new photos, and something waits or not: after so long without a run, the
     * phone holds the app back, which the backup screen says until a run starts in the background.
     */
    @Synchronized
    fun looked(store: Store, waiting: Boolean) {
        val from = pending ?: return
        pending = null
        if (waiting && since(store) == null) set(store, SINCE, from)
    }

    /** Backup turned on or off: what was seen before says nothing of what comes. */
    @Synchronized
    fun forget(store: Store) {
        pending = null
        set(store, SINCE, null)
    }

    // Android's battery optimization

    fun exempt(context: Context): Boolean =
        context.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(context.packageName)

    /** Asks to be left out of battery optimization, or opens the list where that is chosen. */
    fun askExempt(context: Context) {
        val ask = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${context.packageName}"))
        if (!open(context, ask)) open(context, Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
    }

    // The makers' own screens

    /** A screen of the phone's own where an app is let start and run in the background, and what to do there. */
    class Screen(val name: String, val hint: String, private val intent: Intent) {
        fun open(context: Context) {
            if (!open(context, intent)) open(context, details(context))
        }
    }

    private class Known(val name: String, val hint: String, val components: List<String>)

    // Each maker's components, in the order the AutoStarter library tries them, and its setting as
    // dontkillmyapp.com words it; Honor's own package as an Honor phone with MagicOS has it. A phone has
    // only its own maker's packages, which AndroidManifest.xml's <queries> names so that they can be
    // looked up; one not found, or not open to other apps, is passed over.
    private val known = listOf(
        Known(
            "App launch",
            "In App launch, turn Manage automatically off for Cairn, then leave Auto-launch, Secondary launch and Run in background on.",
            listOf(
                "com.hihonor.systemmanager/.startupmgr.ui.StartupNormalAppListActivity",
                "com.huawei.systemmanager/.startupmgr.ui.StartupNormalAppListActivity",
                "com.huawei.systemmanager/.optimize.process.ProtectActivity",
            ),
        ),
        Known(
            "Autostart",
            "In Autostart, turn Cairn on; and in Cairn's app info, set Battery saver to No restrictions.",
            listOf("com.miui.securitycenter/com.miui.permcenter.autostart.AutoStartManagementActivity"),
        ),
        Known(
            "startup manager",
            "In the startup manager, turn Cairn on; and in Cairn's app info, allow Auto Start-up and, under Battery usage, Run in background.",
            listOf(
                "com.coloros.safecenter/.permission.startup.StartupAppListActivity",
                "com.oppo.safe/.permission.startup.StartupAppListActivity",
                "com.coloros.safecenter/.startupapp.StartupAppListActivity",
            ),
        ),
        Known(
            "App Auto-Launch",
            "In App Auto-Launch, turn Cairn on.",
            listOf("com.oneplus.security/.chainlaunch.view.ChainLaunchAppListActivity"),
        ),
        Known(
            "Autostart",
            "In Autostart, turn Cairn on; from Android 13, allow it too under Battery, Background power consumption management.",
            listOf(
                "com.iqoo.secure/.ui.phoneoptimize.AddWhiteListActivity",
                "com.vivo.permissionmanager/.activity.BgStartUpManagerActivity",
                "com.iqoo.secure/.ui.phoneoptimize.BgStartUpManager",
            ),
        ),
        Known(
            "battery settings",
            "Take Cairn out of Sleeping apps and Deep sleeping apps, and add it to Never sleeping apps.",
            listOf(
                "com.samsung.android.lool/com.samsung.android.sm.ui.battery.BatteryActivity",
                "com.samsung.android.lool/com.samsung.android.sm.battery.ui.usage.CheckableAppListActivity",
                "com.samsung.android.lool/com.samsung.android.sm.battery.ui.BatteryActivity",
            ),
        ),
    )

    /** This phone's screen for it, if its maker's is known; the app's details otherwise. */
    fun screen(context: Context): Screen {
        for (k in known) {
            for (c in k.components) {
                val intent = Intent().setComponent(ComponentName.unflattenFromString(c))
                val found = runCatching { context.packageManager.resolveActivity(intent, 0) }.getOrNull()
                if (found?.activityInfo?.exported == true) return Screen(k.name, k.hint, intent)
            }
        }
        return Screen(
            "app info",
            "In Cairn's app info, under Battery or App battery usage, choose Unrestricted or whatever lets it run in the background, and let it start by itself where the phone asks.",
            details(context),
        )
    }

    private fun details(context: Context) =
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${context.packageName}"))

    private fun open(context: Context, intent: Intent): Boolean = try {
        context.startActivity(intent)
        true
    } catch (e: ActivityNotFoundException) {
        false
    } catch (e: SecurityException) {
        false
    }
}
