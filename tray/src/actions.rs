//! What the tray does on the desktop: open a folder, a file or the server's page, deal with a conflict
//! by the ordinary file operations a person would make, which the daemon then syncs, and take a root
//! that has gone off the list.

use crate::hub::Hub;
use cairn_protocol::local::OpenConflict;
use std::path::{Component, Path, PathBuf};

pub fn open(target: &Path) -> Result<(), String> {
    platform_open(target.as_os_str())
}

/// Only an `http` or `https` address, so a registry that says otherwise opens nothing.
pub fn open_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("not a web address: {url}"));
    }
    platform_open(std::ffi::OsStr::new(url))
}

#[cfg(target_os = "macos")]
fn platform_open(target: &std::ffi::OsStr) -> Result<(), String> {
    run(std::process::Command::new("/usr/bin/open").arg(target))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_open(target: &std::ffi::OsStr) -> Result<(), String> {
    run(std::process::Command::new("xdg-open").arg(target))
}

#[cfg(windows)]
fn platform_open(target: &std::ffi::OsStr) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let (verb, file) = (wide("open".as_ref()), wide(target));
    // SAFETY: NUL-terminated strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success.
    if r as usize > 32 {
        Ok(())
    } else {
        Err(format!(
            "Windows could not open {}: error {}",
            target.to_string_lossy(),
            r as usize
        ))
    }
}

/// Takes a root off this device's list of roots, as `cairn forget` does, if the tray shows it as gone:
/// the window asked first.
pub fn forget(hub: &Hub, root_id: &str) -> Result<String, String> {
    let r = hub
        .root(root_id)
        .ok_or("that root is no longer in the registry")?;
    if !crate::model::shown_gone(&r) {
        return Err(format!(
            "{} is not shown as gone, so it stays on the list; `cairn forget --even-if-present` takes off one still there",
            r.entry.path.display()
        ));
    }
    let file = cairn_protocol::local::registry_path()
        .ok_or("no home directory to find the registry in")?;
    crate::registry::forget(&file, &r.entry)
}

/// Shows the file selected in its folder.
pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        run(std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path))
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer reads its own command line: the path in quotes after the comma, nothing escaped.
        let mut arg = std::ffi::OsString::from("/select,\"");
        arg.push(path.as_os_str());
        arg.push("\"");
        std::process::Command::new("explorer.exe")
            .raw_arg(arg)
            .spawn()
            .map(drop)
            .map_err(|e| format!("cannot start Explorer: {e}"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        open(path.parent().unwrap_or(path))
    }
}

/// Where something a daemon reported is: `rel` is relative to the root, by `/`, as `cairn status`
/// prints it. None for a path that would leave the root.
pub fn under(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            name => path.push(name),
        }
    }
    Some(path)
}

/// Shows it selected in its folder or, once it has gone, the nearest folder above it in the root that
/// has not.
pub fn show(root: &Path, path: &Path) -> Result<(), String> {
    if path.symlink_metadata().is_ok() {
        return reveal(path);
    }
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| d.starts_with(root)) {
        if d.is_dir() {
            return open(d);
        }
        dir = d.parent();
    }
    Err(format!(
        "{} is not there, nor is the root's folder {}",
        path.display(),
        root.display()
    ))
}

/// Onto the clipboard, as a person's copy would put it.
pub fn copy_text(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // pbcopy reads by the locale, which a program started by launchd may not have.
        pipe(
            std::process::Command::new("/usr/bin/pbcopy").env("LC_CTYPE", "UTF-8"),
            text,
        )
    }
    #[cfg(windows)]
    {
        windows_clipboard::set(text)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut last = String::new();
        for (program, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            match pipe(std::process::Command::new(program).args(args), text) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(format!("cannot copy: no clipboard program worked ({last})"))
    }
}

#[cfg(unix)]
fn pipe(cmd: &mut std::process::Command, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    let wrote = child.stdin.take().map(|mut i| i.write_all(text.as_bytes()));
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{:?} did not finish: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    match wrote {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(format!("cannot write to {:?}: {e}", cmd.get_program())),
        None => Err(format!("{:?} took no input", cmd.get_program())),
    }
}

#[cfg(windows)]
mod windows_clipboard {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    const CF_UNICODETEXT: u32 = 13;

    pub fn set(text: &str) -> Result<(), String> {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        // Another program may hold the clipboard for a moment.
        let mut tries = 0;
        // SAFETY: opened by no window, and closed below whatever happens in between.
        while unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            tries += 1;
            if tries == 10 {
                return Err(failed("open"));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: the clipboard is open.
        let r = unsafe { put(&wide) };
        // SAFETY: opened above.
        unsafe { CloseClipboard() };
        r
    }

    fn failed(what: &str) -> String {
        format!(
            "cannot {what} the clipboard: {}",
            std::io::Error::last_os_error()
        )
    }

    /// # Safety
    /// The clipboard is open on this thread.
    unsafe fn put(wide: &[u16]) -> Result<(), String> {
        unsafe {
            if EmptyClipboard() == 0 {
                return Err(failed("empty"));
            }
            let mem = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(wide));
            if mem.is_null() {
                return Err(failed("make room on"));
            }
            let at = GlobalLock(mem) as *mut u16;
            if at.is_null() {
                GlobalFree(mem);
                return Err(failed("write to"));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), at, wide.len());
            GlobalUnlock(mem);
            // Once set, the memory is the clipboard's.
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                GlobalFree(mem);
                return Err(failed("write to"));
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn run(cmd: &mut std::process::Command) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// To the Trash or the Recycle Bin, from which a person can take it back.
pub fn to_trash(path: &Path) -> Result<(), String> {
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        // Not through Finder, which asks for permission to be scripted.
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = &mut ctx;
    ctx.delete(path)
        .map_err(|e| format!("cannot move {} to the Trash: {e}", path.display()))
}

/// A rename that fails if the name is taken, so nothing that turned up there meanwhile is replaced.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes());
        let (f, t) = (c(from)?, c(to)?);
        // SAFETY: two NUL-terminated paths.
        if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes());
        let (f, t) = (c(from)?, c(to)?);
        // SAFETY: two NUL-terminated paths, relative to nothing.
        let r = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let w = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
        let (f, t) = (w(from), w(to));
        // SAFETY: two NUL-terminated paths. No MOVEFILE_REPLACE_EXISTING: a taken name fails.
        if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A path as a person writes it, for comparing two and for copying one: on Windows without the `\\?\`
/// that some calls put in front. What is done to a path is done to it as given, since a long path needs
/// that prefix.
#[cfg(windows)]
pub fn plain(p: &Path) -> PathBuf {
    use std::path::Prefix;
    let mut parts = p.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return p.to_path_buf();
    };
    let mut out = match prefix.kind() {
        Prefix::VerbatimDisk(d) => PathBuf::from(format!("{}:\\", d as char)),
        Prefix::VerbatimUNC(server, share) => {
            let mut s = std::ffi::OsString::from(r"\\");
            s.push(server);
            s.push(r"\");
            s.push(share);
            s.push(r"\");
            PathBuf::from(s)
        }
        _ => return p.to_path_buf(),
    };
    for part in parts {
        if part != Component::RootDir {
            out.push(part.as_os_str());
        }
    }
    out
}

#[cfg(not(windows))]
pub fn plain(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// A path the daemon gave, taken only if it lies inside the root the registry names, with no `..`.
fn inside(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let (p, r) = (plain(path), plain(root));
    let ok = path.is_absolute()
        && p.starts_with(&r)
        && path.components().all(|c| c != Component::ParentDir)
        && p != r;
    if ok {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "the daemon named {} for a conflict, outside {}: nothing done",
            path.display(),
            root.display()
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    Show,
    OpenBoth,
    KeepOriginal,
    KeepCopy,
}

impl ConflictAction {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "show" => Self::Show,
            "open_both" => Self::OpenBoth,
            "keep_original" => Self::KeepOriginal,
            "keep_copy" => Self::KeepCopy,
            _ => return None,
        })
    }
}

/// Whether both versions are here and of one kind, so that one can take the other's place.
pub fn can_swap(c: &OpenConflict) -> bool {
    let kind = |p: &Option<PathBuf>| {
        p.as_ref()
            .and_then(|p| std::fs::symlink_metadata(p).ok())
            .map(|m| m.file_type())
    };
    match (kind(&c.copy), kind(&c.original)) {
        (Some(a), Some(b)) => a.is_dir() == b.is_dir() && a.is_symlink() == b.is_symlink(),
        _ => false,
    }
}

/// Acts on conflict `seq` of a root, asking the daemon for the list again first, so that what is done
/// is done to the conflict as it is now. A message for the person on success.
pub fn conflict(
    hub: &Hub,
    root_id: &str,
    seq: u64,
    action: ConflictAction,
) -> Result<String, String> {
    let root = hub
        .root(root_id)
        .ok_or("that root is no longer in the registry")?
        .entry
        .path;
    let list = hub.fetch_conflicts(root_id)?;
    let c = list
        .into_iter()
        .find(|c| c.seq == seq)
        .ok_or("that conflict is no longer open: it was dealt with meanwhile")?;
    let copy = c.copy.as_deref().map(|p| inside(&root, p)).transpose()?;
    let original = c
        .original
        .as_deref()
        .map(|p| inside(&root, p))
        .transpose()?;
    let missing = || {
        format!(
            "the copy, {}, is not on this device: deal with it on one that has it",
            c.copy_display
        )
    };
    match action {
        ConflictAction::Show => {
            reveal(
                copy.as_deref()
                    .or(original.as_deref())
                    .ok_or_else(missing)?,
            )?;
            Ok(String::new())
        }
        ConflictAction::OpenBoth => {
            for p in [&original, &copy].into_iter().flatten() {
                open(p)?;
            }
            Ok(String::new())
        }
        ConflictAction::KeepOriginal => {
            let copy = copy.ok_or_else(missing)?;
            to_trash(&copy)?;
            Ok(format!(
                "{} is in the Trash; the original stays",
                c.copy_display
            ))
        }
        ConflictAction::KeepCopy => {
            let (Some(copy), Some(original)) = (copy, original) else {
                return Err(
                    "both versions must be on this device to put one in the other's place".into(),
                );
            };
            if !can_swap(&c) {
                return Err("the two are not of one kind, so one cannot take the other's place: open both, and move them by hand".into());
            }
            to_trash(&original)?;
            rename_noreplace(&copy, &original).map_err(|e| {
                format!(
                    "the original is in the Trash, but the copy could not take its name ({e}): it is still at {}",
                    plain(&copy).display()
                )
            })?;
            Ok(format!(
                "The copy is now {}, and the original is in the Trash",
                plain(&original).display()
            ))
        }
    }
}

/// Starting at login: a LaunchAgent on macOS, the Run key on Windows, an autostart entry on Linux.
pub mod login {
    #[cfg(target_os = "macos")]
    pub const LABEL: &str = "storage.cairn.tray";
    /// The label, and Cairn Tray.app's bundle id, up to 0.1.23.
    #[cfg(target_os = "macos")]
    const OLD_LABEL: &str = "com.cairn.tray";

    #[cfg(target_os = "macos")]
    fn agent(label: &str) -> Option<std::path::PathBuf> {
        std::env::var_os("HOME").map(|h| {
            std::path::Path::new(&h)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"))
        })
    }

    #[cfg(target_os = "macos")]
    fn plist() -> Option<std::path::PathBuf> {
        agent(LABEL)
    }

    /// The agent of a tray up to 0.1.23, if it starts a tray: README then named a daemon's agent
    /// com.cairn.<root>, so one for a root called "tray" has its name, and is left alone.
    #[cfg(target_os = "macos")]
    fn old_is_ours() -> bool {
        agent(OLD_LABEL)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|t| t.contains("/cairn-tray</string>"))
    }

    #[cfg(target_os = "macos")]
    fn remove(path: &std::path::Path) -> Result<(), String> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("cannot remove {}: {e}", path.display()))
            }
            _ => Ok(()),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn enabled() -> bool {
        plist().is_some_and(|p| p.exists()) || old_is_ours()
    }

    /// A tray updated from 0.1.23 or before, set to start at login, has its agent under the old label,
    /// which names a bundle id there no longer is, so Login Items cannot show it as Cairn Tray: written
    /// anew under this one. The old job stays loaded until the session ends, and starts nothing after.
    #[cfg(target_os = "macos")]
    pub fn carry_over() -> Option<Result<String, String>> {
        old_is_ours().then(|| {
            set(true).map(|()| format!("start at login moved from {OLD_LABEL} to {LABEL}"))
        })
    }

    /// Restarted after a crash, not after Quit, and only in a desktop session. The label is also Cairn
    /// Tray.app's bundle id, which the plist names so that Login Items shows the app with its icon.
    #[cfg(target_os = "macos")]
    pub fn set(on: bool) -> Result<(), String> {
        let path = plist().ok_or("HOME is not set")?;
        let old = agent(OLD_LABEL).filter(|_| old_is_ours());
        if !on {
            remove(&path)?;
            return old.map_or(Ok(()), |p| remove(&p));
        }
        let exe = std::env::current_exe().map_err(|e| format!("cannot tell where this is: {e}"))?;
        let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{exe}</string></array>
    <key>AssociatedBundleIdentifiers</key><array><string>{LABEL}</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            exe = esc(&exe.to_string_lossy()),
            log = esc(&format!("{home}/Library/Logs/cairn-tray.log")),
        );
        let dir = path.parent().ok_or("no LaunchAgents folder")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let tmp = path.with_extension("plist.new");
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        old.map_or(Ok(()), |p| remove(&p))
    }

    #[cfg(windows)]
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    #[cfg(windows)]
    const VALUE: &str = "Cairn Tray";
    /// The value's name up to 0.1.21, when the tray was called "Cairn", now the main program's name. A
    /// copy updated from one still starts by it: it is the tray's while it names cairn-tray.exe, and is
    /// removed when the tray is set either way.
    #[cfg(windows)]
    const OLD_VALUE: &str = "Cairn";

    #[cfg(windows)]
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// What a value of the Run key holds, None when it is not there; empty when it is there but would not
    /// read.
    #[cfg(windows)]
    fn read(name: &str) -> Option<String> {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
        };
        let (key, value) = (wide(RUN_KEY), wide(name));
        let mut len = 0u32;
        // SAFETY: NUL-terminated names; asks only for the size.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if r != 0 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: NUL-terminated names, and a buffer of the length given, in bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if r != 0 {
            return Some(String::new());
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    #[cfg(windows)]
    fn old_is_ours() -> bool {
        read(OLD_VALUE).is_some_and(|d| d.to_ascii_lowercase().contains("cairn-tray.exe"))
    }

    #[cfg(windows)]
    fn delete(name: &str) -> Result<(), String> {
        use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};
        let (key, value) = (wide(RUN_KEY), wide(name));
        // SAFETY: NUL-terminated names.
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
        // 2: it was not there.
        if r == 0 || r == 2 {
            Ok(())
        } else {
            Err(format!("cannot change the Run key: error {r}"))
        }
    }

    #[cfg(windows)]
    pub fn enabled() -> bool {
        read(VALUE).is_some() || old_is_ours()
    }

    #[cfg(windows)]
    pub fn set(on: bool) -> Result<(), String> {
        use windows_sys::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
        if old_is_ours() {
            delete(OLD_VALUE)?;
        }
        if !on {
            return delete(VALUE);
        }
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
        let exe = std::env::current_exe().map_err(|e| format!("cannot tell where this is: {e}"))?;
        let data = wide(&format!("\"{}\"", exe.display()));
        // SAFETY: NUL-terminated names, and data of the length given, in bytes.
        let r = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(format!("cannot change the Run key: error {r}"))
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn desktop_file() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config"))
            })?;
        Some(base.join("autostart/cairn-tray.desktop"))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn enabled() -> bool {
        desktop_file().is_some_and(|p| p.exists())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn set(on: bool) -> Result<(), String> {
        let path = desktop_file().ok_or("HOME is not set")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(format!("cannot remove {}: {e}", path.display()))
                }
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().map_err(|e| format!("cannot tell where this is: {e}"))?;
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=Cairn Tray\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A change is found under its root by its parts, in this system's own way of joining them.
    #[test]
    fn a_change_is_found_under_its_root() {
        let root = Path::new("root");
        let want: PathBuf = ["root", "docs", "a b.txt"].iter().collect();
        assert_eq!(under(root, "docs/a b.txt"), Some(want.clone()));
        assert_eq!(under(root, "./docs//a b.txt"), Some(want));
        assert_eq!(under(root, ""), Some(root.to_path_buf()));
        assert_eq!(under(root, "docs/../../x"), None);
    }

    /// Shown in its folder where it is, or the nearest folder above it in the root once it has gone.
    #[test]
    fn a_change_that_has_gone_shows_its_folder() {
        let root = std::env::temp_dir().join(format!("cairn-tray-show-{}", std::process::id()));
        let gone = root.join("gone").join("deeper").join("x.txt");
        std::fs::create_dir_all(&root).unwrap();
        let outside = root.parent().unwrap().join("not-the-root");
        let r = show(&root.join("missing-root"), &outside);
        assert!(r.is_err(), "{r:?}");
        let _ = std::fs::remove_dir_all(&root);
        // Nothing of it is left, the root's folder included: said, not opened.
        let r = show(&root, &gone);
        assert!(r.is_err(), "{r:?}");
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_path_compares_as_the_plain_one() {
        let root = Path::new(r"E:\c");
        assert_eq!(
            plain(Path::new(r"\\?\E:\c\a.txt")),
            Path::new(r"E:\c\a.txt")
        );
        assert!(inside(root, Path::new(r"\\?\E:\c\a.txt")).is_ok());
        assert!(inside(Path::new(r"\\?\E:\c"), Path::new(r"E:\c\a.txt")).is_ok());
        assert!(inside(root, Path::new(r"\\?\E:\d\a.txt")).is_err());
        assert_eq!(
            plain(Path::new(r"\\?\UNC\srv\share\x")),
            Path::new(r"\\srv\share\x")
        );
    }

    #[test]
    fn a_rename_never_replaces_what_is_at_the_name() {
        let dir = std::env::temp_dir().join(format!("cairn-tray-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        assert!(rename_noreplace(&a, &b).is_err());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
        rename_noreplace(&a, &c).unwrap();
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "a");
        assert!(!a.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_outside_the_root_is_refused() {
        let root = if cfg!(windows) {
            Path::new(r"E:\c")
        } else {
            Path::new("/r/c")
        };
        let ok = root.join("a").join("b.txt");
        assert!(inside(root, &ok).is_ok());
        assert!(inside(root, &root.join("..").join("x")).is_err());
        assert!(inside(root, root).is_err());
        assert!(inside(root, Path::new("b.txt")).is_err());
        let sibling = root.parent().unwrap().join("cd").join("x");
        assert!(inside(root, &sibling).is_err());
    }
}
