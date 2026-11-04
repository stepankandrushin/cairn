//! What the tray does on the desktop: open a folder, a file or the server's page, and deal with a
//! conflict by the ordinary file operations a person would make, which the daemon then syncs.

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

/// A path as a person writes it, for comparing two: on Windows without the `\\?\` that some calls put
/// in front. What is done to a path is done to it as given, since a long path needs that prefix.
#[cfg(windows)]
fn plain(p: &Path) -> PathBuf {
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
fn plain(p: &Path) -> PathBuf {
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
    pub const LABEL: &str = "com.cairn.tray";

    #[cfg(target_os = "macos")]
    fn plist() -> Option<std::path::PathBuf> {
        std::env::var_os("HOME").map(|h| {
            std::path::Path::new(&h)
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist"))
        })
    }

    #[cfg(target_os = "macos")]
    pub fn enabled() -> bool {
        plist().is_some_and(|p| p.exists())
    }

    /// Restarted after a crash, not after Quit, and only in a desktop session.
    #[cfg(target_os = "macos")]
    pub fn set(on: bool) -> Result<(), String> {
        let path = plist().ok_or("HOME is not set")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(format!("cannot remove {}: {e}", path.display()))
                }
                _ => Ok(()),
            };
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
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    #[cfg(windows)]
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    #[cfg(windows)]
    const VALUE: &str = "Cairn";

    #[cfg(windows)]
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[cfg(windows)]
    pub fn enabled() -> bool {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
        };
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
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
        r == 0
    }

    #[cfg(windows)]
    pub fn set(on: bool) -> Result<(), String> {
        use windows_sys::Win32::System::Registry::{
            RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
        };
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
        if !on {
            // SAFETY: NUL-terminated names.
            let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
            // 2: it was not there.
            return if r == 0 || r == 2 {
                Ok(())
            } else {
                Err(format!("cannot change the Run key: error {r}"))
            };
        }
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
            "[Desktop Entry]\nType=Application\nName=Cairn\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n",
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
