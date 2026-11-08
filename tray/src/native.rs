//! What the tray's menu needs from each system that Tauri does not give: whether a person has it open,
//! so that it is not set anew under them, and a right click on an item of its Recent activity, which
//! copies that item's path where a click shows it.
//!
//! Setting a menu anew while it is open closes it on Windows, as the one shown is destroyed, and on
//! macOS leaves its items pointing at what was freed; the hub's state changes every few seconds while
//! a daemon syncs, so the menu closed under a person's pointer.

use std::sync::atomic::{AtomicI64, Ordering};

/// When the menu opened, in seconds; 0 while it is closed.
static OPENED: AtomicI64 = AtomicI64::new(0);

/// Open longer than this, the menu is taken to be closed whatever was last heard, so that a close the
/// system never told of cannot leave the menu as it was for good.
const OPEN_AT_MOST: i64 = 600;

pub fn menu_open() -> bool {
    let at = OPENED.load(Ordering::SeqCst);
    at != 0 && crate::model::now_secs() - at < OPEN_AT_MOST
}

#[cfg(any(windows, target_os = "macos"))]
fn opened(open: bool) {
    let at = if open {
        crate::model::now_secs().max(1)
    } else {
        0
    };
    OPENED.store(at, Ordering::SeqCst);
}

pub use imp::{recent_menu, take_secondary, watch};

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::OnceLock;
    use tauri::menu::{ContextMenu, Menu};
    use tauri::Wry;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, EndMenu, GetSubMenu, SetWindowsHookExW, CWPSTRUCT, WH_CALLWNDPROC,
        WM_ENTERMENULOOP, WM_EXITMENULOOP, WM_MENURBUTTONUP,
    };

    /// The Recent activity submenu as Windows shows it, for telling a right click on it from others.
    static RECENT: AtomicIsize = AtomicIsize::new(0);
    static ON_RIGHT_CLICK: OnceLock<fn(usize)> = OnceLock::new();

    /// On the thread that owns the tray's window, which is the one the menu runs on. Windows tells that
    /// window, by messages sent to it, when a menu opens and closes and when the right button is let go
    /// over an item: a hook on the thread sees them before tray-icon's window does.
    pub fn watch(on_right_click: fn(usize)) -> Result<(), String> {
        let _ = ON_RIGHT_CLICK.set(on_right_click);
        // SAFETY: a hook on this thread alone, whose procedure is this program's and lives as long.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_CALLWNDPROC,
                Some(sent),
                std::ptr::null_mut(),
                GetCurrentThreadId(),
            )
        };
        if hook.is_null() {
            return Err(format!(
                "cannot watch the menu: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    unsafe extern "system" fn sent(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // HC_ACTION: lparam is the message.
        if code == 0 {
            // SAFETY: for WH_CALLWNDPROC, lparam points at a CWPSTRUCT for the length of the call.
            let m = unsafe { &*(lparam as *const CWPSTRUCT) };
            match m.message {
                WM_ENTERMENULOOP => super::opened(true),
                WM_EXITMENULOOP => super::opened(false),
                // wParam is the item's place in the menu that lParam names.
                WM_MENURBUTTONUP if m.lParam != 0 && m.lParam == RECENT.load(Ordering::SeqCst) => {
                    if let Some(f) = ON_RIGHT_CLICK.get() {
                        f(m.wParam);
                    }
                    // SAFETY: closes this thread's menu, as a click on an item would.
                    unsafe { EndMenu() };
                }
                _ => {}
            }
        }
        // SAFETY: passes the message on unchanged, as every hook must.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    /// Once `menu` is the tray's: muda puts a submenu's own handle into its parent's, at `at`.
    pub fn recent_menu(menu: &Menu<Wry>, at: usize) {
        let sub = match menu.hpopupmenu() {
            // SAFETY: a handle the menu owns, which outlives the call.
            Ok(h) => unsafe { GetSubMenu(h as _, at as i32) as isize },
            Err(e) => {
                crate::log(&format!(
                    "cannot find the menu's Recent activity, so a right click there does nothing: {e}"
                ));
                0
            }
        };
        RECENT.store(sub, Ordering::SeqCst);
    }

    pub fn take_secondary() -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSEvent, NSEventModifierFlags, NSEventType,
        NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification,
        NSMenuWillSendActionNotification,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use tauri::menu::Menu;
    use tauri::Wry;

    /// Menus being tracked, the open submenus among them should macOS say so of each.
    static DEPTH: AtomicI64 = AtomicI64::new(0);
    /// The item last chosen was chosen by a right click, a Control-click or an Option-click.
    static SECONDARY: AtomicBool = AtomicBool::new(false);

    /// On the main thread. Tauri hands a chosen item on later, by its event loop, when the event that
    /// chose it is gone, so how it was clicked is read as macOS says the menu is about to act.
    pub fn watch(_on_right_click: fn(usize)) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("the menu is watched from the main thread")?;
        let center = NSNotificationCenter::defaultCenter();
        let begin = RcBlock::new(|_: NonNull<NSNotification>| {
            DEPTH.fetch_add(1, Ordering::SeqCst);
            super::opened(true);
        });
        let end = RcBlock::new(|_: NonNull<NSNotification>| {
            let left = DEPTH.fetch_sub(1, Ordering::SeqCst) - 1;
            if left <= 0 {
                DEPTH.store(0, Ordering::SeqCst);
                super::opened(false);
            }
        });
        let action = RcBlock::new(move |_: NonNull<NSNotification>| {
            let event = NSApplication::sharedApplication(mtm).currentEvent();
            let right = event.is_some_and(|e| {
                let t = e.r#type();
                t == NSEventType::RightMouseUp || t == NSEventType::RightMouseDown
            });
            let keys = NSEvent::modifierFlags_class().0
                & (NSEventModifierFlags::Control.0 | NSEventModifierFlags::Option.0);
            SECONDARY.store(right || keys != 0, Ordering::SeqCst);
        });
        // SAFETY: notifications of every menu, delivered on the thread that posts them, which for a
        // menu is the main one; the blocks keep nothing that is not theirs. The center keeps each
        // observer for as long as this program runs, so the tokens, only for taking one off, go.
        unsafe {
            for (name, block) in [
                (NSMenuDidBeginTrackingNotification, &begin),
                (NSMenuDidEndTrackingNotification, &end),
                (NSMenuWillSendActionNotification, &action),
            ] {
                let token = center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    None,
                    block,
                );
                std::mem::forget(token);
            }
        }
        Ok(())
    }

    pub fn recent_menu(_menu: &Menu<Wry>, _at: usize) {}

    /// Whether the item being handled was chosen by a right click, or a Control- or Option-click.
    pub fn take_secondary() -> bool {
        SECONDARY.swap(false, Ordering::SeqCst)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use tauri::menu::Menu;
    use tauri::Wry;

    pub fn watch(_on_right_click: fn(usize)) -> Result<(), String> {
        Ok(())
    }

    pub fn recent_menu(_menu: &Menu<Wry>, _at: usize) {}

    pub fn take_secondary() -> bool {
        false
    }
}
