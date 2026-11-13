//! macOS alone: the daemon known to the system as the app of its bundle.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::Path;

/// Have LaunchServices know this daemon as the app of the bundle it runs from, Cairn Sync.app, as it
/// knows the tray by Cairn Tray.app. Activity Monitor gives a process its bundle's name and icon only
/// once LaunchServices knows it so, which AppKit does for an app; launchd starts the daemon without
/// AppKit, so it showed as its bare program name, beside the tray's "Cairn Tray". `Ok(false)` when
/// this is not the place.
///
/// TransformProcessType checks the process in, as the background-only app its Info.plist makes it
/// (LSBackgroundOnly), and then has nothing to change: no Dock icon, menu or window. As a UI element,
/// which the tray is, it would be brought to the front when someone opens the bundle, and with no event
/// loop to answer, it would keep the focus from every other app; a background-only app is never brought
/// forward. Opening the bundle while the daemon runs now starts nothing, where it used to start the
/// CLI, which printed its usage. The bundle's identifier and signature are as they were, and with them each
/// permission macOS keeps by them. Every command maps ApplicationServices, which the frameworks it links
/// bring, and dyld leaves it uninitialized until it is used: opened here, for the daemon alone, it costs
/// the daemon about 2 MB of footprint and no other command anything.
///
/// At logout loginwindow sends each app a quit event, which here finds no one to take it, so loginwindow
/// kills the process and goes on, as launchd would have at the end of the session: a kill at any moment
/// is the crash a sync already recovers from. Activity Monitor's Quit is that event too, so it takes Force
/// Quit there, after which a LaunchAgent that keeps the daemon alive starts it again.
///
/// HIServices aborts a process that LaunchServices does not take in, so this is done only where a
/// desktop is, in a session with graphic access, as Activity Monitor's is: not over SSH, nor for a daemon
/// of the system's. And only from the bundle itself: through a link, as `~/.local/bin/cairn-sync`,
/// macOS finds no bundle and would name the process for the link. Nothing depends on it, so a failure is only said.
pub fn check_in() -> Result<bool, String> {
    // The path as started, not the file it leads to: where macOS looks for the bundle.
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot tell where this program is: {e}"))?;
    if !in_bundle(&exe) || !on_a_desktop()? {
        return Ok(false);
    }
    const RTLD_LAZY: c_int = 0x1;
    const RTLD_LOCAL: c_int = 0x4;
    // HIServices' Processes.h.
    #[repr(C)]
    struct ProcessSerialNumber {
        high_long_of_psn: u32,
        low_long_of_psn: u32,
    }
    const K_CURRENT_PROCESS: u32 = 2;
    const K_PROCESS_TRANSFORM_TO_BACKGROUND_APPLICATION: u32 = 2;
    // What it answers when the process is of that type already, as LSBackgroundOnly makes this one.
    const PARAM_ERR: i32 = -50;
    type TransformProcessType = unsafe extern "C" fn(*const ProcessSerialNumber, u32) -> i32;
    extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlerror() -> *const c_char;
    }
    let why = || {
        // SAFETY: libSystem's own; null, or a C string valid until the next dl call on this thread.
        let e = unsafe { dlerror() };
        if e.is_null() {
            "no reason given".to_string()
        } else {
            // SAFETY: as above, and read before any other dl call.
            unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
        }
    };
    // SAFETY: a system framework, loaded for good: never closed, as the check-in lasts as long as the
    // process does.
    let lib = unsafe {
        dlopen(
            c"/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices"
                .as_ptr(),
            RTLD_LAZY | RTLD_LOCAL,
        )
    };
    if lib.is_null() {
        return Err(format!("cannot load ApplicationServices: {}", why()));
    }
    // SAFETY: a handle dlopen gave, and a symbol name that is a C string.
    let f = unsafe { dlsym(lib, c"TransformProcessType".as_ptr()) };
    if f.is_null() {
        return Err(format!("no TransformProcessType: {}", why()));
    }
    // SAFETY: TransformProcessType has this signature (Processes.h), and the framework stays loaded.
    let transform: TransformProcessType = unsafe { std::mem::transmute(f) };
    let psn = ProcessSerialNumber {
        high_long_of_psn: 0,
        low_long_of_psn: K_CURRENT_PROCESS,
    };
    // SAFETY: a valid serial number, for this process, which the call only reads.
    match unsafe { transform(&psn, K_PROCESS_TRANSFORM_TO_BACKGROUND_APPLICATION) } {
        0 | PARAM_ERR => Ok(true),
        e => Err(format!("TransformProcessType failed with error {e}")),
    }
}

/// What libmalloc reads once, as a process starts: `0` has it keep no cache of freed large blocks.
const LARGE_CACHE: &str = "MallocLargeCache";

/// Start the daemon again in this same process, with the allocator keeping no cache of freed large blocks.
/// Without that, macOS 26 and 27 keep the blocks of 64 KiB and more that a sync frees, for a later
/// allocation, and `malloc_zone_pressure_relief` hands none of them back. So an idle daemon held much of
/// its biggest sync's peak: 0.8 to 1.9 GB on Macs syncing a tree of 568k entries, of which about 1 MB
/// was still allocated, and 15 MB with the cache off, where a block goes back to the system when it is
/// freed. Its cost is a fresh mapping for each such allocation. The variable is read before
/// `main`, so it has to be in the environment the program starts with: this sets it and runs the same
/// program, with the same arguments, in this same process, where launchd sees nothing end. A
/// variable that is set already, to any value, is left as it is, so this happens at most once.
/// Returns only when that could not be done, with why, and the daemon goes on as it is.
pub fn without_large_cache() -> Option<String> {
    use std::os::unix::process::CommandExt;
    if std::env::var_os(LARGE_CACHE).is_some() {
        return None;
    }
    // The path as started, as `check_in` needs it, and the same first argument.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return Some(format!("cannot tell where this program is: {e}")),
    };
    let mut args = std::env::args_os();
    let first = args.next().unwrap_or_else(|| exe.clone().into_os_string());
    let err = std::process::Command::new(&exe)
        .arg0(first)
        .args(args)
        .env(LARGE_CACHE, "0")
        .exec();
    Some(format!("cannot start {} again: {err}", exe.display()))
}

/// Whether this process's security session has graphic access: a person's desktop, not SSH's session
/// or the system's.
fn on_a_desktop() -> Result<bool, String> {
    // Security's AuthSession.h.
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
    }
    let mut attributes = 0u32;
    // SAFETY: the caller's own session, asked for its attributes alone; a null ID is not written.
    let status = unsafe {
        SessionGetInfo(
            CALLER_SECURITY_SESSION,
            std::ptr::null_mut(),
            &mut attributes,
        )
    };
    if status != 0 {
        return Err(format!("SessionGetInfo failed with error {status}"));
    }
    Ok(attributes & SESSION_HAS_GRAPHIC_ACCESS != 0)
}

/// `<Name>.app/Contents/MacOS/<program>`.
fn in_bundle(exe: &Path) -> bool {
    let named = |p: Option<&Path>, n: &str| p.and_then(Path::file_name).is_some_and(|f| f == n);
    let macos = exe.parent();
    let contents = macos.and_then(Path::parent);
    let app = contents.and_then(Path::parent);
    named(macos, "MacOS")
        && named(contents, "Contents")
        && app.and_then(Path::extension).is_some_and(|e| e == "app")
}

#[cfg(test)]
mod tests {
    use super::in_bundle;
    use std::path::Path;

    #[test]
    fn only_a_program_in_a_bundle() {
        let yes = "/Users/x/Applications/Cairn Sync.app/Contents/MacOS/cairn-sync";
        assert!(in_bundle(Path::new(yes)));
        for no in [
            "/Users/x/.local/bin/cairn-sync",
            "/Users/x/Cairn Sync/Contents/MacOS/cairn-sync",
            "/Users/x/Cairn Sync.app/Contents/cairn-sync",
            "/MacOS/cairn-sync",
            "cairn-sync",
        ] {
            assert!(!in_bundle(Path::new(no)), "{no}");
        }
    }
}
