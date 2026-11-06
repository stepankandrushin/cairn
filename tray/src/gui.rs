//! The tray icon, its menu and the window, through Tauri. Everything here only shows the hub's state
//! and passes a person's choice on to the daemon or to `actions`.

use crate::actions::{self, ConflictAction};
use crate::hub::{Hub, Link, State};
use crate::icon;
use crate::link;
use crate::model::{self, Light, Problem};
use crate::settings::{self, Clock};
use cairn_protocol::local::{Activity, Request};
use serde::Serialize;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
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
            forget,
            open_folder,
            open_server,
            set_clock
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
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
    login: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct RootLine {
    id: String,
    name: String,
    line: String,
    up: bool,
    paused_by_you: bool,
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
        login: actions::login::enabled(),
    }
}

/// Follows the hub, and redraws the icon and menu when what they show has changed: rebuilding a menu
/// a person has open would close it.
fn refresh(app: AppHandle, tray: TrayIcon) {
    let mut version = 0;
    let mut shown: Option<MenuModel> = None;
    loop {
        version = hub().wait_past(version, Duration::from_secs(2));
        let m = menu_model(&hub().snapshot());
        if shown.as_ref() == Some(&m) {
            continue;
        }
        let light_changed = shown.as_ref().map(|s| s.light) != Some(m.light);
        let (a, t, model) = (app.clone(), tray.clone(), m.clone());
        let _ = app.run_on_main_thread(move || {
            if light_changed {
                let _ = t.set_icon(Some(image(model.light)));
            }
            let _ = t.set_tooltip(Some(&model.tooltip));
            match build_menu(&a, &model) {
                Ok(menu) => {
                    let _ = t.set_menu(Some(menu));
                }
                Err(e) => crate::log(&format!("cannot build the menu: {e}")),
            }
        });
        shown = Some(m);
    }
}

fn clip(s: &str) -> String {
    const MAX: usize = 90;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

fn root_items(app: &AppHandle, r: &RootLine) -> tauri::Result<Vec<MenuItem<Wry>>> {
    let pause = if r.paused_by_you {
        MenuItem::with_id(
            app,
            format!("resume:{}", r.id),
            "Resume",
            r.up,
            None::<&str>,
        )?
    } else {
        MenuItem::with_id(app, format!("pause:{}", r.id), "Pause", r.up, None::<&str>)?
    };
    Ok(vec![
        MenuItem::with_id(
            app,
            format!("sync:{}", r.id),
            "Sync now",
            r.up,
            None::<&str>,
        )?,
        pause,
        MenuItem::with_id(
            app,
            format!("folder:{}", r.id),
            "Open folder",
            true,
            None::<&str>,
        )?,
        MenuItem::with_id(
            app,
            format!("web:{}", r.id),
            "Open in browser",
            true,
            None::<&str>,
        )?,
    ])
}

fn build_menu(app: &AppHandle, m: &MenuModel) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    let line = |id: String, text: &str| MenuItem::with_id(app, id, clip(text), false, None::<&str>);
    if let Some(e) = &m.registry_error {
        menu.append(&line("line:registry".into(), e)?)?;
    }
    for r in &m.roots {
        menu.append(&line(format!("line:{}", r.id), &r.line)?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    match m.roots.as_slice() {
        [] => {}
        [one] => {
            for item in root_items(app, one)? {
                menu.append(&item)?;
            }
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        many => {
            for r in many {
                let items = root_items(app, r)?;
                let refs: Vec<&dyn IsMenuItem<Wry>> =
                    items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
                menu.append(&Submenu::with_items(app, &r.name, true, &refs)?)?;
            }
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
    }
    let count = |word: &str, n: usize| {
        if n == 0 {
            word.to_string()
        } else {
            format!("{word} ({n})")
        }
    };
    menu.append(&MenuItem::with_id(
        app,
        "recent",
        "Recent activity",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "problems",
        count("Problems", m.problems),
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "conflicts",
        count("Conflicts", m.conflicts),
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "settings",
        "Settings…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        "login",
        "Start at login",
        true,
        m.login,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit Cairn",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

fn on_menu(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref().to_string();
    let (what, root) = id.split_once(':').unwrap_or((id.as_str(), ""));
    let (what, root) = (what.to_string(), root.to_string());
    match what.as_str() {
        "quit" => app.exit(0),
        "recent" | "problems" | "conflicts" | "settings" => show_window(app, &what),
        "login" => {
            let on = !actions::login::enabled();
            if let Err(e) = actions::login::set(on) {
                hub().notice(e);
                show_window(app, "status");
            }
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
        "folder" | "web" => {
            let r = match what.as_str() {
                "folder" => open_folder_of(&root),
                _ => open_server_of(&root),
            };
            if let Err(e) = r {
                hub().notice(e);
                show_window(app, "status");
            }
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
            .title("Cairn")
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
    roots: Vec<UiRoot>,
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

/// Takes a root shown as gone off the registry, as `cairn forget` does; the window asked first.
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

/// The window's next snapshot shows the new clock; the menu's next redraw, within two seconds, too.
#[tauri::command]
async fn set_clock(clock: Clock) -> Result<(), String> {
    blocking(move || settings::set_clock(clock)).await
}
