//! The tray icon, its menu and the window, through Tauri. Everything here only shows the hub's state
//! and passes a person's choice on to the daemon or to `actions`.

use crate::actions::{self, ConflictAction};
use crate::hub::{Hub, Link, State};
use crate::icon;
use crate::link;
use crate::model::{self, Light, Problem};
use crate::native;
use crate::settings::{self, Clock};
use cairn_protocol::local::{
    self as local_api, Activity, Change, DeletedItem, Direction, Reply, Request, MAX_RECENT,
};
use cairn_protocol::update::Policy;
use cairn_protocol::ObjectKind;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{
    IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu,
};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry};

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();

fn hub() -> &'static Arc<Hub> {
    HUB.get().expect("the hub starts before the window")
}

pub fn run(hub: Arc<Hub>, open: Option<String>) -> Result<(), String> {
    let _ = HUB.set(hub);
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            snapshot,
            act,
            refresh_conflicts,
            conflict,
            deleted,
            undelete,
            forget,
            open_folder,
            open_server,
            show_change,
            copy_change_path,
            set_login,
            set_clock,
            set_update_policy,
            update_now,
            close_window
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            // Before the tray, on the thread its menu runs on. Without it the menu still works, but may
            // be set anew while open, and a right click copies nothing.
            if let Err(e) = native::watch(right_click) {
                crate::log(&e);
            }
            let tray = TrayIconBuilder::with_id("cairn")
                .icon(image(Light::NotRunning))
                .icon_as_template(false)
                .tooltip("Cairn")
                .show_menu_on_left_click(true)
                .on_menu_event(on_menu)
                .build(app)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || refresh(handle, tray));
            if let Some(tab) = &open {
                show_window(app.handle(), tab);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| format!("cannot start the tray: {e}"))?;
    app.run(|_, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            // The window closing is no reason to go: only Quit is.
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
    Ok(())
}

fn image(light: Light) -> Image<'static> {
    Image::new_owned(icon::draw(light), icon::SIZE, icon::SIZE)
}

#[derive(Clone, Debug, PartialEq)]
struct MenuModel {
    light: Light,
    tooltip: String,
    registry_error: Option<String>,
    roots: Vec<RootLine>,
    problems: usize,
    conflicts: usize,
    /// A release that is out and not installed here.
    update: Option<String>,
    recent: Vec<RecentLine>,
    /// A daemon is running, so an empty `recent` means nothing has synced since it started.
    any_up: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct RootLine {
    id: String,
    name: String,
    line: String,
    up: bool,
    paused_by_you: bool,
}

/// A change in the menu's Recent activity: a click shows it in its folder, a right click copies its
/// path.
#[derive(Clone, Debug, PartialEq)]
struct RecentLine {
    text: String,
    root: PathBuf,
    /// Where the change left it: for a move, the new name.
    path: PathBuf,
}

fn worst(s: &State, now: i64) -> Light {
    if s.roots.is_empty() {
        return Light::NotRunning;
    }
    s.roots
        .iter()
        .map(|r| model::light(r, now))
        .max()
        .unwrap_or(Light::NotRunning)
}

fn menu_model(s: &State) -> MenuModel {
    let now = model::now_secs();
    let light = worst(s, now);
    let roots: Vec<RootLine> = s
        .roots
        .iter()
        .map(|r| RootLine {
            id: r.entry.root_id.clone(),
            name: model::name(r),
            line: format!("{}: {}", model::name(r), model::headline(r, now)),
            up: matches!(r.link, Link::Up(_)),
            paused_by_you: matches!(&r.link, Link::Up(st) if st.paused_since.is_some()),
        })
        .collect();
    let tooltip = match roots.as_slice() {
        [] => "Cairn: no roots on this device".to_string(),
        [one] => format!("Cairn: {}", one.line),
        many => format!("Cairn: {} roots, {}", many.len(), light.word()),
    };
    MenuModel {
        light,
        tooltip,
        registry_error: s.registry_error.clone(),
        problems: s.roots.iter().map(|r| model::problems(r).len()).sum(),
        conflicts: s.roots.iter().filter_map(model::open_conflicts).sum(),
        roots,
        update: crate::update::status()
            .ok()
            .flatten()
            .and_then(|s| s.available),
        recent: recent_lines(s),
        any_up: s.roots.iter().any(|r| matches!(r.link, Link::Up(_))),
    }
}

/// Every root's, newest first: each daemon keeps its last [`MAX_RECENT`], and the menu shows as many.
fn recent_lines(s: &State) -> Vec<RecentLine> {
    let many = s.roots.len() > 1;
    let mut all: Vec<(i64, RecentLine)> = Vec::new();
    for r in &s.roots {
        let Link::Up(st) = &r.link else { continue };
        for a in &st.recent {
            let rel = shown_path(a);
            let Some(path) = actions::under(&r.entry.path, rel) else {
                continue;
            };
            let shown = if many {
                format!("{}/{rel}", model::name(r))
            } else {
                rel.to_string()
            };
            all.push((
                a.time,
                RecentLine {
                    text: format!(
                        "{}  {} {}  ·  {}",
                        model::when(a.time),
                        match a.direction {
                            Direction::Up => "↑",
                            Direction::Down => "↓",
                        },
                        model::clip_middle(&shown, 64),
                        change_word(a.change)
                    ),
                    root: r.entry.path.clone(),
                    path,
                },
            ));
        }
    }
    // Stable, so each daemon's own order stands within a second.
    all.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    all.truncate(MAX_RECENT);
    all.into_iter().map(|(_, l)| l).collect()
}

/// Where the change left it, relative to its root: a move is shown by its new name.
fn shown_path(a: &Activity) -> &str {
    match (&a.change, &a.to) {
        (Change::Moved, Some(to)) => to,
        _ => &a.path,
    }
}

/// As the window's Recent activity says it.
fn change_word(c: Change) -> &'static str {
    match c {
        Change::Added => "added",
        Change::Changed => "changed",
        Change::Metadata => "metadata changed",
        Change::Moved => "moved",
        Change::Removed => "removed",
        Change::Conflict => "conflict copy made",
    }
}

/// Set when a menu could not be set because it was open, so the next round sets it.
static UNSET: AtomicBool = AtomicBool::new(false);

/// Follows the hub, and redraws the icon and menu when what they show has changed, but never the menu
/// while a person has it open: setting it anew closes it (`native`). It is set once it has closed.
fn refresh(app: AppHandle, tray: TrayIcon) {
    let mut version = 0;
    let mut shown: Option<MenuModel> = None;
    let mut restarting = false;
    loop {
        version = hub().wait_past(version, Duration::from_secs(2));
        // Once: a restart that fails is said, and this tray goes on as it is.
        if !restarting && crate::update::replaced() {
            restarting = true;
            crate::log("this tray's program has been replaced, by an update or by hand: starting the new one");
            let e = crate::update::restart();
            crate::log(&e);
            hub().notice(format!(
                "A new version of the tray is installed, and {e}: quit and start it again"
            ));
        }
        if UNSET.swap(false, Ordering::SeqCst) {
            shown = None;
        }
        let m = menu_model(&hub().snapshot());
        if shown.as_ref() == Some(&m) || native::menu_open() {
            continue;
        }
        let light_changed = shown.as_ref().map(|s| s.light) != Some(m.light);
        let (a, t, model) = (app.clone(), tray.clone(), m.clone());
        let _ = app.run_on_main_thread(move || {
            if light_changed {
                let _ = t.set_icon(Some(image(model.light)));
            }
            let _ = t.set_tooltip(Some(&model.tooltip));
            // Asked again on the thread the menu opens on, as it may have opened since.
            if native::menu_open() {
                UNSET.store(true, Ordering::SeqCst);
                return;
            }
            let n = MENU_NUMBER.fetch_add(1, Ordering::SeqCst) + 1;
            match build_menu(&a, &model, n) {
                Ok((menu, recent_at)) => match t.set_menu(Some(menu.clone())) {
                    Ok(()) => {
                        native::recent_menu(&menu, recent_at);
                        let mut shown = RECENT.lock().unwrap_or_else(|e| e.into_inner());
                        shown.push((n, model.recent.clone()));
                        let extra = shown.len().saturating_sub(2);
                        shown.drain(..extra);
                    }
                    Err(e) => crate::log(&format!("cannot set the menu: {e}")),
                },
                Err(e) => crate::log(&format!("cannot build the menu: {e}")),
            }
        });
        shown = Some(m);
    }
}

/// Each menu set is numbered, and its items of Recent activity carry the number.
static MENU_NUMBER: AtomicU64 = AtomicU64::new(0);

/// The Recent activity of the menu set last, and of the one before, for a click that was on its way as
/// the menu was set anew: by the menu's number.
static RECENT: Mutex<Vec<(u64, Vec<RecentLine>)>> = Mutex::new(Vec::new());

fn recent_line(n: u64, i: usize) -> Option<RecentLine> {
    let shown = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    shown
        .iter()
        .find(|(m, _)| *m == n)
        .and_then(|(_, lines)| lines.get(i).cloned())
}

/// A right click on the `i`th item of Recent activity in the menu that is open, which is the one set last,
/// as none is set while open. Windows says it from inside the menu.
fn right_click(i: usize) {
    let line = {
        let shown = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        shown.last().and_then(|(_, lines)| lines.get(i).cloned())
    };
    if let Some(line) = line {
        std::thread::spawn(move || copy_path(&line.path));
    }
}

/// As a person would write it, so on Windows without the `\\?\` the registry's root may have.
fn copy_path(path: &Path) {
    if let Err(e) = actions::copy_text(&actions::plain(path).to_string_lossy()) {
        hub().notice(e);
    }
}

fn item(app: &AppHandle, id: impl Into<MenuId>, text: &str, on: bool) -> tauri::Result<Kind> {
    Ok(Kind::MenuItem(MenuItem::with_id(
        app,
        id,
        model::menu_label(text),
        on,
        None::<&str>,
    )?))
}

type Kind = MenuItemKind<Wry>;

fn root_items(app: &AppHandle, r: &RootLine) -> tauri::Result<Vec<Kind>> {
    let pause = if r.paused_by_you {
        item(app, format!("resume:{}", r.id), "Resume", r.up)?
    } else {
        item(app, format!("pause:{}", r.id), "Pause", r.up)?
    };
    Ok(vec![
        item(app, format!("sync:{}", r.id), "Sync now", r.up)?,
        pause,
        item(app, format!("folder:{}", r.id), "Open folder", true)?,
        item(app, format!("web:{}", r.id), "Open in browser", true)?,
    ])
}

fn recent_menu(app: &AppHandle, m: &MenuModel, n: u64) -> tauri::Result<Submenu<Wry>> {
    let sub = Submenu::with_id(app, "recent", "Recent activity", true)?;
    if m.recent.is_empty() {
        let why = if m.any_up {
            "Nothing synced since the daemon started"
        } else {
            "No daemon is running to ask"
        };
        sub.append(&item(app, "recent:none", why, false)?)?;
    }
    for (i, r) in m.recent.iter().enumerate() {
        sub.append(&item(app, format!("recent:{n}:{i}"), &r.text, true)?)?;
    }
    Ok(sub)
}

/// The menu, and where its Recent activity is in it: Settings nearest the icon, then Recent activity and
/// the rest a person deals with, then each root's actions, then what each root is doing, laid out by
/// `model::from_icon`; Quit last.
fn build_menu(app: &AppHandle, m: &MenuModel, n: u64) -> tauri::Result<(Menu<Wry>, usize)> {
    let count = |word: &str, n: usize| {
        if n == 0 {
            word.to_string()
        } else {
            format!("{word} ({n})")
        }
    };
    let mut settings = vec![item(app, "settings", "Settings…", true)?];
    if let Some(v) = &m.update {
        settings.push(item(
            app,
            "settings",
            &format!("Cairn {v} is available…"),
            true,
        )?);
    }
    let deal = vec![
        Kind::Submenu(recent_menu(app, m, n)?),
        item(app, "problems", &count("Problems", m.problems), true)?,
        item(app, "conflicts", &count("Conflicts", m.conflicts), true)?,
        item(app, "deleted", "Recently deleted…", true)?,
    ];
    let actions = match m.roots.as_slice() {
        [] => Vec::new(),
        [one] => root_items(app, one)?,
        many => {
            let mut subs = Vec::new();
            for r in many {
                let items = root_items(app, r)?;
                let refs: Vec<&dyn IsMenuItem<Wry>> =
                    items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
                subs.push(Kind::Submenu(Submenu::with_items(
                    app,
                    model::menu_label(&r.name),
                    true,
                    &refs,
                )?));
            }
            subs
        }
    };
    let mut lines = Vec::new();
    if let Some(e) = &m.registry_error {
        lines.push(item(app, "line:registry", e, false)?);
    }
    for r in &m.roots {
        lines.push(item(app, format!("line:{}", r.id), &r.line, false)?);
    }
    let sections = model::from_icon([settings, deal, actions, lines], cfg!(target_os = "macos"));
    let menu = Menu::new(app)?;
    let mut at = 0;
    let mut recent_at = 0;
    for s in sections.iter().filter(|s| !s.is_empty()) {
        if at > 0 {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
            at += 1;
        }
        for i in s {
            if i.id().as_ref() == "recent" {
                recent_at = at;
            }
            menu.append(i)?;
            at += 1;
        }
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&item(app, "quit", "Quit Cairn Tray", true)?)?;
    Ok((menu, recent_at))
}

fn on_menu(app: &AppHandle, event: MenuEvent) {
    // Taken for every item, so what macOS said of one click is not read for another.
    let secondary = native::take_secondary();
    let id = event.id().as_ref().to_string();
    let (what, root) = id.split_once(':').unwrap_or((id.as_str(), ""));
    let (what, root) = (what.to_string(), root.to_string());
    match what.as_str() {
        "quit" => app.exit(0),
        "problems" | "conflicts" | "deleted" | "settings" => show_window(app, &what),
        "recent" => {
            let line = root
                .split_once(':')
                .and_then(|(n, i)| Some((n.parse().ok()?, i.parse().ok()?)))
                .and_then(|(n, i)| recent_line(n, i));
            let Some(line) = line else { return };
            let app = app.clone();
            std::thread::spawn(move || {
                if secondary {
                    copy_path(&line.path);
                } else if let Err(e) = actions::show(&line.root, &line.path) {
                    hub().notice(e);
                    show_window(&app, "recent");
                }
            });
        }
        "sync" | "pause" | "resume" => {
            let request = match what.as_str() {
                "sync" => Request::SyncNow,
                "pause" => Request::Pause,
                _ => Request::Resume,
            };
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = link::tell(&root, request) {
                    hub().notice(format!("The daemon did not take that: {e}"));
                    show_window(&app, "status");
                }
            });
        }
        "folder" => {
            if let Err(e) = open_folder_of(&root) {
                hub().notice(e);
                show_window(app, "status");
            }
        }
        "web" => {
            // Off the event loop: it waits on the daemon, which asks the server.
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = open_server_of(&root) {
                    hub().notice(e);
                    show_window(&app, "status");
                }
            });
        }
        _ => {}
    }
}

fn show_window(app: &AppHandle, tab: &str) {
    hub().ask_tab(tab);
    let app = app.clone();
    // Off the event loop: building a webview from inside its handler can wait on that same loop.
    std::thread::spawn(move || {
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
            return;
        }
        let built = WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
            .title("Cairn Tray")
            .inner_size(780.0, 560.0)
            .min_inner_size(480.0, 320.0)
            .build();
        match built {
            Ok(w) => {
                let _ = w.set_focus();
            }
            Err(e) => crate::log(&format!("cannot open the window: {e}")),
        }
    });
}

fn open_folder_of(root_id: &str) -> Result<(), String> {
    let r = hub()
        .root(root_id)
        .ok_or("that root is no longer in the registry")?;
    actions::open(&r.entry.path)
}

fn open_server_of(root_id: &str) -> Result<(), String> {
    let r = hub()
        .root(root_id)
        .ok_or("that root is no longer in the registry")?;
    let url = match (&r.entry.server, &r.link) {
        (s, _) if !s.is_empty() => s.clone(),
        (_, Link::Up(st)) if !st.server.is_empty() => st.server.clone(),
        _ => return Err("this root names no server".into()),
    };
    // Signed in as this root's token, which the daemon asks the server for, so the page asks for none.
    let why = match link::ask(root_id, Request::SignInLink, Duration::from_secs(20)) {
        Ok(Reply::SignInLink { url }) => return actions::open_url(&url),
        Ok(Reply::Error { text }) => text,
        Ok(other) => format!("the daemon answered {other:?}"),
        Err(e) => e.to_string(),
    };
    hub().notice(format!(
        "The server's page opened without signing in, so it asks for a token: {why}"
    ));
    actions::open_url(&url)
}

#[derive(Serialize)]
struct UiState {
    now: i64,
    light: Light,
    light_word: &'static str,
    registry: Option<String>,
    registry_error: Option<String>,
    tab: (u64, String),
    notice: (u64, String),
    clock: Clock,
    /// Why `tray.json` could not be read, so its defaults are in use.
    settings_error: Option<String>,
    settings_path: Option<String>,
    /// The tray starts at login.
    login: bool,
    /// What a click on a change opens, for saying so.
    file_manager: &'static str,
    update: UiUpdate,
    roots: Vec<UiRoot>,
}

/// What the updater last wrote, and how updates are set.
#[derive(Serialize)]
struct UiUpdate {
    policy: Option<Policy>,
    tray_version: &'static str,
    checked_when: Option<String>,
    latest: Option<String>,
    available: Option<String>,
    waits: Option<String>,
    installed: Option<String>,
    installed_when: Option<String>,
    error: Option<String>,
    error_when: Option<String>,
    rejected: Vec<String>,
    /// Why the settings or the updater's state could not be read.
    problem: Option<String>,
    /// A daemon is running to ask.
    can_ask: bool,
}

fn ui_update(s: &State) -> UiUpdate {
    let (policy, policy_problem) = match crate::update::policy() {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e)),
    };
    let (st, state_problem) = match crate::update::status() {
        Ok(st) => (st.unwrap_or_default(), None),
        Err(e) => (Default::default(), Some(e)),
    };
    let when = |t: i64| (t > 0).then(|| model::when(t));
    UiUpdate {
        policy,
        tray_version: env!("CARGO_PKG_VERSION"),
        checked_when: when(st.checked),
        latest: st.latest,
        available: st.available,
        waits: st.waits,
        installed: st.installed,
        installed_when: when(st.installed_at),
        error_when: st.error.as_ref().and_then(|_| when(st.error_since)),
        error: st.error,
        rejected: st.rejected,
        problem: policy_problem.or(state_problem),
        can_ask: s.roots.iter().any(|r| matches!(r.link, Link::Up(_))),
    }
}

#[derive(Serialize)]
struct UiRoot {
    root_id: String,
    name: String,
    path: String,
    server: String,
    light: Light,
    light_word: &'static str,
    headline: String,
    up: bool,
    /// Shown as gone, so Forget is offered.
    gone: bool,
    paused_by_you: bool,
    version: String,
    problems: Vec<Problem>,
    recent: Vec<UiActivity>,
    conflicts: Option<UiConflicts>,
}

/// Each time goes to the window as `model::when` writes it, so the window, the menu and `--probe` show
/// it on the same clock.
#[derive(Serialize)]
struct UiActivity {
    #[serde(flatten)]
    activity: Activity,
    when: String,
}

#[derive(Serialize)]
struct UiConflicts {
    at: i64,
    at_when: String,
    error: Option<String>,
    open: Vec<UiConflict>,
}

#[derive(Serialize)]
struct UiConflict {
    seq: u64,
    time: i64,
    when: String,
    device_name: String,
    copy_display: String,
    original_display: Option<String>,
    copy_here: bool,
    original_here: bool,
    can_swap: bool,
}

#[tauri::command]
async fn snapshot() -> UiState {
    let s = hub().snapshot();
    let now = model::now_secs();
    let light = worst(&s, now);
    let update = ui_update(&s);
    let roots = s
        .roots
        .iter()
        .map(|r| {
            let (up, paused_by_you, version, recent) = match &r.link {
                Link::Up(st) => (
                    true,
                    st.paused_since.is_some(),
                    st.version.clone(),
                    st.recent
                        .iter()
                        .map(|a| UiActivity {
                            activity: a.clone(),
                            when: model::when(a.time),
                        })
                        .collect(),
                ),
                _ => (false, false, String::new(), Vec::new()),
            };
            let l = model::light(r, now);
            UiRoot {
                root_id: r.entry.root_id.clone(),
                name: model::name(r),
                path: r.entry.path.display().to_string(),
                server: r.entry.server.clone(),
                light: l,
                light_word: l.word(),
                headline: model::headline(r, now),
                up,
                gone: model::shown_gone(r),
                paused_by_you,
                version,
                problems: model::problems(r),
                recent,
                conflicts: r.conflicts.as_ref().map(|(at, c)| match c {
                    Err(e) => UiConflicts {
                        at: *at,
                        at_when: model::when(*at),
                        error: Some(e.clone()),
                        open: Vec::new(),
                    },
                    Ok(list) => UiConflicts {
                        at: *at,
                        at_when: model::when(*at),
                        error: None,
                        open: list
                            .iter()
                            .map(|c| UiConflict {
                                seq: c.seq,
                                time: c.time,
                                when: model::when(c.time),
                                device_name: c.device_name.clone(),
                                copy_display: c.copy_display.clone(),
                                original_display: c.original_display.clone(),
                                copy_here: c
                                    .copy
                                    .as_ref()
                                    .is_some_and(|p| p.symlink_metadata().is_ok()),
                                original_here: c
                                    .original
                                    .as_ref()
                                    .is_some_and(|p| p.symlink_metadata().is_ok()),
                                can_swap: actions::can_swap(c),
                            })
                            .collect(),
                    },
                }),
            }
        })
        .collect();
    UiState {
        now,
        light,
        light_word: light.word(),
        registry: s.registry.map(|p| p.display().to_string()),
        registry_error: s.registry_error,
        tab: s.tab,
        notice: s.notice,
        clock: settings::clock(),
        settings_error: settings::error(),
        settings_path: settings::path().map(|p| p.display().to_string()),
        login: actions::login::enabled(),
        file_manager: if cfg!(target_os = "macos") {
            "Finder"
        } else if cfg!(windows) {
            "Explorer"
        } else {
            "its folder"
        },
        update,
        roots,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn act(root_id: String, what: String) -> Result<(), String> {
    blocking(move || {
        let request = match what.as_str() {
            "sync_now" => Request::SyncNow,
            "pause" => Request::Pause,
            "resume" => Request::Resume,
            _ => return Err(format!("no such request: {what}")),
        };
        link::tell(&root_id, request)
    })
    .await
}

#[tauri::command]
async fn refresh_conflicts(root_id: String) -> Result<(), String> {
    blocking(move || hub().fetch_conflicts(&root_id).map(drop)).await
}

#[tauri::command]
async fn conflict(root_id: String, seq: u64, what: String) -> Result<String, String> {
    blocking(move || {
        let action = ConflictAction::parse(&what).ok_or(format!("no such action: {what}"))?;
        actions::conflict(hub(), &root_id, seq, action)
    })
    .await
}

/// What the daemon lists as deleted, and when it said so.
#[derive(Serialize)]
struct UiDeletedList {
    at: i64,
    at_when: String,
    head: u64,
    items: Vec<UiDeleted>,
}

#[derive(Serialize)]
struct UiDeleted {
    #[serde(flatten)]
    item: DeletedItem,
    when: String,
    /// "4.2 MiB", or for a folder "12 files, 4.2 MiB".
    size: String,
}

/// A daemon reads the server's history to answer, all of it the first time: as long as that takes.
const DELETED_WAIT: Duration = Duration::from_secs(600);

#[tauri::command]
async fn deleted(root_id: String, days: u32) -> Result<UiDeletedList, String> {
    blocking(move || {
        match link::ask(
            &root_id,
            Request::Deleted { days: Some(days) },
            DELETED_WAIT,
        ) {
            Ok(Reply::Deleted { head, items }) => {
                let at = model::now_secs();
                Ok(UiDeletedList {
                    at,
                    at_when: model::when(at),
                    head,
                    items: items
                        .into_iter()
                        .map(|item| UiDeleted {
                            when: model::when(item.time),
                            size: match item.kind {
                                ObjectKind::Dir => format!(
                                    "{} file{}, {}",
                                    item.files,
                                    if item.files == 1 { "" } else { "s" },
                                    local_api::bytes(item.bytes)
                                ),
                                ObjectKind::Symlink => "a symlink".into(),
                                ObjectKind::File => local_api::bytes(item.bytes),
                            },
                            item,
                        })
                        .collect(),
                })
            }
            Ok(Reply::Error { text }) => Err(text),
            Ok(other) => Err(format!("the daemon answered {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
}

/// Brings these back through the daemon, which then syncs; says what came back and where.
#[tauri::command]
async fn undelete(root_id: String, ids: Vec<String>) -> Result<String, String> {
    blocking(
        move || match link::ask(&root_id, Request::Undelete { ids }, DELETED_WAIT) {
            Ok(Reply::Undeleted { items, already, .. }) => {
                let mut lines = Vec::new();
                if items.len() > 3 {
                    lines.push(format!("Brought back {} items.", items.len()));
                }
                for u in &items {
                    if u.renamed {
                        lines.push(format!(
                            "{} is back as {}, as its name is taken now.",
                            u.was, u.path
                        ));
                    } else if items.len() <= 3 {
                        lines.push(format!("{} is back.", u.path));
                    }
                }
                if !already.is_empty() {
                    lines.push(format!("Not deleted any more: {}.", already.join(", ")));
                }
                match items.len() {
                    0 => {}
                    1 => lines.push("Each device gets it with its next sync.".into()),
                    _ => lines.push("Each device gets them with its next sync.".into()),
                }
                Ok(lines.join(" "))
            }
            Ok(Reply::Error { text }) => Err(text),
            Ok(other) => Err(format!("the daemon answered {other:?}")),
            Err(e) => Err(e.to_string()),
        },
    )
    .await
}

/// Takes a root shown as gone off the registry, as `cairn-sync forget` does; the window asked first.
#[tauri::command]
async fn forget(root_id: String) -> Result<String, String> {
    blocking(move || actions::forget(hub(), &root_id)).await
}

#[tauri::command]
async fn open_folder(root_id: String) -> Result<(), String> {
    blocking(move || open_folder_of(&root_id)).await
}

#[tauri::command]
async fn open_server(root_id: String) -> Result<(), String> {
    blocking(move || open_server_of(&root_id)).await
}

/// Where a change in Recent activity is: `path` relative to the root, as the daemon said it.
fn change_path(root_id: &str, path: &str) -> Result<(PathBuf, PathBuf), String> {
    let r = hub()
        .root(root_id)
        .ok_or("that root is no longer in the registry")?;
    let full = actions::under(&r.entry.path, path)
        .ok_or_else(|| format!("{path} is not a path inside the root"))?;
    Ok((r.entry.path, full))
}

#[tauri::command]
async fn show_change(root_id: String, path: String) -> Result<(), String> {
    blocking(move || {
        let (root, full) = change_path(&root_id, &path)?;
        actions::show(&root, &full)
    })
    .await
}

#[tauri::command]
async fn copy_change_path(root_id: String, path: String) -> Result<String, String> {
    blocking(move || {
        let (_, full) = change_path(&root_id, &path)?;
        let text = actions::plain(&full).to_string_lossy().into_owned();
        actions::copy_text(&text)?;
        Ok(format!("Copied {text}"))
    })
    .await
}

#[tauri::command]
async fn set_login(on: bool) -> Result<(), String> {
    blocking(move || actions::login::set(on)).await
}

/// For every program of this user here: the daemons' updater reads it.
#[tauri::command]
async fn set_update_policy(policy: Policy) -> Result<(), String> {
    blocking(move || crate::update::set_policy(policy)).await
}

/// Asks the first daemon that answers: there is one updater for all of them.
#[tauri::command]
async fn update_now() -> Result<String, String> {
    blocking(move || {
        let s = hub().snapshot();
        let up: Vec<_> = s
            .roots
            .iter()
            .filter(|r| matches!(r.link, Link::Up(_)))
            .map(|r| r.entry.root_id.clone())
            .collect();
        if up.is_empty() {
            return Err("no daemon is running here to update from: `cairn-sync update now` does it".into());
        }
        let mut last = String::new();
        for id in up {
            match link::tell(&id, Request::UpdateNow) {
                Ok(()) => {
                    return Ok("Asked: the daemons and the tray restart into a new version as it is installed, and this tab says how it went".into())
                }
                Err(e) => last = e,
            }
        }
        Err(format!("no daemon took it: {last}"))
    })
    .await
}

/// The window's next snapshot shows the new clock; the menu's next redraw, within two seconds, too.
#[tauri::command]
async fn set_clock(clock: Clock) -> Result<(), String> {
    blocking(move || settings::set_clock(clock)).await
}

/// Escape in the window, as its × does: closed, and built anew the next time it is asked for.
#[tauri::command]
async fn close_window(window: tauri::WebviewWindow) -> Result<(), String> {
    window
        .close()
        .map_err(|e| format!("cannot close the window: {e}"))
}
