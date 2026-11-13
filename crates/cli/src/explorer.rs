//! `cairn explorer`: the badges of cairn-explorer's DLL in Windows Explorer, turned on and off.
//!
//! cairn.exe carries the DLL, as a release builds it (scripts/release-windows.ps1, `CAIRN_EXPLORER_DLL`),
//! and the four icons. Turned on, they are written to a folder of their own under the user's Cairn
//! folder, named by what is in them, so a new version never writes over a DLL that Explorer has loaded:
//! it goes beside it, and the handlers' classes, which are this user's (HKCU), point at it. Explorer
//! lists its overlay handlers for the machine (HKLM), which takes an administrator once: the handlers'
//! names and classes, which no version changes. A daemon that starts on a new version moves the
//! classes to its own DLL; Explorer takes it up when it next starts, and keeps the one it has loaded
//! until then.

use anyhow::{anyhow, bail, Context, Result};
use cairn_protocol::local::{user_dir, OVERLAYS};
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteKeyW, RegDeleteTreeW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHCNE_ASSOCCHANGED,
    SHCNF_IDLIST, SHELLEXECUTEINFOW,
};

/// The DLL as this build carries it: empty for a build that was not given one.
const DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cairn-explorer.dll"));
const DLL_NAME: &str = "cairn-explorer.dll";

const ICONS: [(&str, &[u8]); 4] = [
    (
        "error.ico",
        include_bytes!("../../explorer/icons/error.ico"),
    ),
    (
        "warning.ico",
        include_bytes!("../../explorer/icons/warning.ico"),
    ),
    (
        "syncing.ico",
        include_bytes!("../../explorer/icons/syncing.ico"),
    ),
    (
        "synced.ico",
        include_bytes!("../../explorer/icons/synced.ico"),
    ),
];

/// Where Explorer lists its overlay handlers, read in this order of names.
const HANDLERS: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ShellIconOverlayIdentifiers";
/// Explorer has 15 overlays, and keeps 4 of them for its own.
const SHOWN: usize = 11;

fn class_key(clsid: &str) -> String {
    format!(r"Software\Classes\CLSID\{clsid}")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(code: u32) -> io::Result<()> {
    match code {
        0 => Ok(()),
        c => Err(io::Error::from_raw_os_error(c as i32)),
    }
}

fn set(hive: HKEY, key: &str, name: Option<&str>, value: &str) -> io::Result<()> {
    let (key, value) = (wide(key), wide(value));
    let name = name.map(wide);
    // SAFETY: terminated strings, and the data's length in bytes, its terminator in it.
    check(unsafe {
        RegSetKeyValueW(
            hive,
            key.as_ptr(),
            name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    })
}

fn get(hive: HKEY, key: &str, name: Option<&str>) -> io::Result<Option<String>> {
    let key = wide(key);
    let name = name.map(wide);
    let name_ptr = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
    let mut len = 0u32;
    // SAFETY: terminated strings; first only the length asked for, then a buffer of that length.
    unsafe {
        let r = RegGetValueW(
            hive,
            key.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        );
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(r)?;
        let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
        let mut len = (buf.len() * 2) as u32;
        check(RegGetValueW(
            hive,
            key.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        ))?;
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..end])))
    }
}

/// A key and everything under it; one that is not there is no error.
fn delete(hive: HKEY, key: &str) -> io::Result<()> {
    let key = wide(key);
    // SAFETY: a terminated name.
    let r = unsafe { RegDeleteTreeW(hive, key.as_ptr()) };
    if r != ERROR_FILE_NOT_FOUND {
        check(r)?;
    }
    // SAFETY: as above.
    let r = unsafe { RegDeleteKeyW(hive, key.as_ptr()) };
    if r != ERROR_FILE_NOT_FOUND {
        check(r)?;
    }
    Ok(())
}

/// The names of a key's subkeys, in the order Explorer takes overlay handlers by.
fn subkeys(hive: HKEY, key: &str) -> io::Result<Vec<String>> {
    let key = wide(key);
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: a terminated name, and a key closed below.
    check(unsafe { RegOpenKeyExW(hive, key.as_ptr(), 0, KEY_READ, &mut h) })?;
    let mut out = Vec::new();
    for i in 0.. {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: a buffer of the length given.
        let r = unsafe {
            RegEnumKeyExW(
                h,
                i,
                buf.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if r != 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    // SAFETY: opened above.
    unsafe { RegCloseKey(h) };
    out.sort_by_key(|n| n.to_uppercase());
    Ok(out)
}

/// Where this user's copies of the DLL and its icons go, each version in a folder of its own.
fn base() -> Result<PathBuf> {
    Ok(user_dir()
        .ok_or_else(|| anyhow!("LOCALAPPDATA is not set, so there is no Cairn folder"))?
        .join("explorer"))
}

/// The DLL and the icons, written to their folder unless they are there already: each file written
/// under another name first, so one cut short is never taken for whole.
fn put(dll: &[u8]) -> Result<PathBuf> {
    let mut all = dll.to_vec();
    for (_, icon) in ICONS {
        all.extend_from_slice(icon);
    }
    let dir = base()?.join(&cairn_protocol::Hash::of(&all).to_string()[..16]);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
    for (name, bytes) in std::iter::once((DLL_NAME, dll)).chain(ICONS) {
        let path = dir.join(name);
        if std::fs::read(&path).is_ok_and(|have| have == bytes) {
            continue;
        }
        let part = dir.join(format!("{name}.part"));
        std::fs::write(&part, bytes).with_context(|| format!("cannot write {}", part.display()))?;
        std::fs::rename(&part, &path)
            .with_context(|| format!("cannot put {} in place", path.display()))?;
    }
    Ok(dir.join(DLL_NAME))
}

/// The other versions' folders, those Explorer does not hold: one it holds goes another time.
fn sweep(keep: &Path) {
    let Some(keep) = keep.parent() else {
        return;
    };
    let Ok(base) = base() else {
        return;
    };
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        if e.path() != keep {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// This user's classes, pointing at this DLL.
fn register_classes(dll: &Path) -> Result<()> {
    let dll = dll
        .to_str()
        .ok_or_else(|| anyhow!("{} is not valid Unicode", dll.display()))?;
    for o in OVERLAYS {
        let key = class_key(o.clsid);
        set(
            HKEY_CURRENT_USER,
            &key,
            None,
            &format!("Cairn badge: {:?}", o.badge).to_lowercase(),
        )?;
        let server = format!(r"{key}\InprocServer32");
        set(HKEY_CURRENT_USER, &server, None, dll)?;
        set(
            HKEY_CURRENT_USER,
            &server,
            Some("ThreadingModel"),
            "Apartment",
        )?;
    }
    Ok(())
}

/// The DLL this user's classes point at now, if they are registered.
fn registered_dll() -> Option<PathBuf> {
    let key = format!(r"{}\InprocServer32", class_key(OVERLAYS[0].clsid));
    get(HKEY_CURRENT_USER, &key, None)
        .ok()
        .flatten()
        .map(PathBuf::from)
}

/// The machine's list of overlay handlers, with these or without them: what takes an administrator.
pub fn set_handlers(on: bool) -> io::Result<()> {
    for o in OVERLAYS {
        let key = format!(r"{HANDLERS}\{}", o.key);
        match on {
            true => set(HKEY_LOCAL_MACHINE, &key, None, o.clsid)?,
            false => delete(HKEY_LOCAL_MACHINE, &key)?,
        }
    }
    Ok(())
}

fn handlers_listed() -> bool {
    OVERLAYS.iter().all(|o| {
        get(HKEY_LOCAL_MACHINE, &format!(r"{HANDLERS}\{}", o.key), None)
            .ok()
            .flatten()
            .is_some_and(|c| c.eq_ignore_ascii_case(o.clsid))
    })
}

/// As an administrator: here if this process is one, or else by this program run again elevated, which
/// Windows asks the person at the desktop to allow.
fn handlers_as_administrator(on: bool) -> Result<()> {
    match set_handlers(on) {
        Ok(()) => return Ok(()),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {}
        Err(e) => return Err(e).context("cannot change Explorer's list of overlay handlers"),
    }
    let exe = std::env::current_exe()?;
    let (exe, verb) = (wide(&exe.to_string_lossy()), wide("runas"));
    let args = wide(&format!(
        "explorer handlers {}",
        if on { "on" } else { "off" }
    ));
    // SAFETY: a zeroed struct with its size and terminated strings that outlive the call, and the
    // process handle it gives back, waited for and closed.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = args.as_ptr();
        if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
            let e = io::Error::last_os_error();
            bail!(
                "Explorer's list of overlay handlers takes an administrator, and running as one was not allowed ({e}): run `cairn explorer {}` from a PowerShell run as administrator",
                if on { "on" } else { "off" }
            );
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if code != 0 {
            bail!("changing Explorer's list of overlay handlers as administrator failed (exit code {code})");
        }
    }
    Ok(())
}

/// Explorer looks at every icon again: what is already loaded shows at once, and new handlers once
/// Explorer starts again.
fn refresh_icons() {
    // SAFETY: an event with no items.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
}

pub fn on(dll: Option<&Path>) -> Result<()> {
    let bytes = match dll {
        Some(p) => std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))?,
        None if DLL.is_empty() => bail!(
            "this build of cairn carries no Explorer handler: a release does, or give one built with \
             `cargo build --profile explorer -p cairn-explorer` by --dll"
        ),
        None => DLL.to_vec(),
    };
    let path = put(&bytes)?;
    register_classes(&path)?;
    handlers_as_administrator(true)?;
    sweep(&path);
    refresh_icons();
    println!("Explorer's badges are on, from {}", path.display());
    println!("Explorer shows them once it starts again: sign out and in, or restart Explorer from Task Manager");
    order_warning();
    Ok(())
}

pub fn off() -> Result<()> {
    handlers_as_administrator(false)?;
    for o in OVERLAYS {
        delete(HKEY_CURRENT_USER, &class_key(o.clsid))?;
    }
    // What Explorer still holds stays until it starts again, and goes next time.
    if let Ok(base) = base() {
        for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    refresh_icons();
    println!("Explorer's badges are off; Explorer drops them once it starts again");
    Ok(())
}

/// Where these come among Explorer's overlay handlers, if past those it shows.
fn order_warning() {
    let Ok(names) = subkeys(HKEY_LOCAL_MACHINE, HANDLERS) else {
        return;
    };
    let late: Vec<(usize, &str)> = OVERLAYS
        .iter()
        .filter_map(|o| Some((names.iter().position(|n| n == o.key)?, o.key.trim())))
        .filter(|(i, _)| *i >= SHOWN)
        .collect();
    if !late.is_empty() {
        println!(
            "Explorer shows the first {SHOWN} overlay handlers by name, and {} come after them; other programs' that come first: {}",
            late.iter().map(|(_, k)| *k).collect::<Vec<_>>().join(", "),
            names[..SHOWN.min(names.len())].join(", ")
        );
    }
}

pub fn status() -> Result<()> {
    let dll = registered_dll();
    let listed = handlers_listed();
    match (&dll, listed) {
        (None, false) => println!("off"),
        (Some(d), true) if d.is_file() => println!("on, from {}", d.display()),
        (Some(d), true) => println!("on, but its DLL is gone: {}; `cairn explorer on` puts it back", d.display()),
        (Some(_), false) => println!("half on: this user's classes are registered, but Explorer does not list them; `cairn explorer on` finishes it"),
        (None, true) => println!("half on: Explorer lists the handlers, but this user has no classes for them; `cairn explorer on` finishes it"),
    }
    if let Ok(names) = subkeys(HKEY_LOCAL_MACHINE, HANDLERS) {
        println!("Explorer's overlay handlers, the first {SHOWN} shown:");
        for (i, n) in names.iter().enumerate() {
            println!("{:>3}{} {n:?}", i + 1, if i < SHOWN { " " } else { "x" });
        }
    }
    let embedded = match DLL.is_empty() {
        true => "none".to_string(),
        false => cairn_protocol::Hash::of(DLL).to_string()[..16].to_string(),
    };
    println!("this build carries handler {embedded}");
    Ok(())
}

/// As a daemon starts: if the badges are on, this version's DLL in place of an older one's. Says what
/// it did, or why not; nothing if they are off or already this version's.
pub fn follow_version() -> Option<String> {
    let current = registered_dll()?;
    let base = base().ok()?;
    if DLL.is_empty() || !current.starts_with(&base) {
        return None;
    }
    let line = match put(DLL).and_then(|path| {
        if path == current {
            return Ok(None);
        }
        register_classes(&path)?;
        sweep(&path);
        Ok(Some(path))
    }) {
        Ok(None) => return None,
        Ok(Some(path)) => format!(
            "Explorer's badges now come from this version's handler, {}, once Explorer starts again",
            path.display()
        ),
        Err(e) => format!("Explorer's badges stay with the handler they had, as this version's could not be put in place: {e:#}"),
    };
    Some(line)
}

/// What Windows' own overlay manager, which Explorer uses, gives each path, asked a few times as the
/// handler takes a moment to learn a folder: for trying the handlers without Explorer.
pub fn probe(paths: &[PathBuf]) -> Result<()> {
    use std::ffi::c_void;
    use windows_sys::core::GUID;
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    const CLSID_OVERLAY_MANAGER: GUID = GUID::from_u128(0x63b51f81_c868_11d0_999c_00c04fd655e1);
    const IID_OVERLAY_MANAGER: GUID = GUID::from_u128(0xf10b5e34_dd3b_42a7_aa7d_2f4ec54bb09b);
    type GetFileOverlayInfo =
        unsafe extern "system" fn(*mut c_void, *const u16, u32, *mut i32, u32) -> i32;
    // SAFETY: COM as documented: initialised for this thread, an interface pointer used through its
    // table, whose fourth entry is GetFileOverlayInfo.
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut manager: *mut c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_OVERLAY_MANAGER,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_OVERLAY_MANAGER,
            &mut manager,
        );
        if hr != 0 || manager.is_null() {
            bail!("cannot make Explorer's overlay manager: {hr:#x}");
        }
        let table = *(manager as *const *const usize);
        let get: GetFileOverlayInfo = std::mem::transmute(*table.add(3));
        // LoadNonloadedOverlayIdentifiers: every handler listed, not only those already taken up.
        let load: unsafe extern "system" fn(*mut c_void) -> i32 =
            std::mem::transmute(*table.add(6));
        let loaded = load(manager);
        for path in paths {
            let p = wide(&path.to_string_lossy());
            let attrib = if path.is_dir() { 0x10 } else { 0x80 };
            let mut said = String::new();
            for _ in 0..20 {
                let mut index = -1;
                // SIOM_OVERLAYINDEX
                let hr = get(manager, p.as_ptr(), attrib, &mut index, 1);
                said = match hr {
                    0 => format!("overlay {index}"),
                    hr => format!("none ({hr:#x})"),
                };
                if hr == 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            println!("{}: {said}", path.display());
        }
        let name = wide(DLL_NAME);
        let held =
            !windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(name.as_ptr()).is_null();
        println!(
            "the handlers loaded ({loaded:#x}); {DLL_NAME} {} in this process",
            if held { "is" } else { "is not" }
        );
    }
    Ok(())
}

/// What each of a DLL's handlers says of each path, the DLL loaded here as Explorer loads it but
/// without the registry: the badge, as the first handler to say yes, and its icon.
pub fn probe_dll(dll: &Path, paths: &[PathBuf], again: Option<u64>) -> Result<()> {
    use std::ffi::c_void;
    use windows_sys::core::GUID;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    const IID_ICLASSFACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);
    const IID_HANDLER: GUID = GUID::from_u128(0x0c6c4200_c589_11d0_999a_00c04fd655e1);
    type GetClassObject =
        unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;
    type CreateInstance =
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> i32;
    type IsMemberOf = unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32;
    type GetOverlayInfo =
        unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut i32, *mut u32) -> i32;
    let parse = |clsid: &str| {
        let hex: String = clsid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        GUID::from_u128(u128::from_str_radix(&hex, 16).expect("a class ID"))
    };
    // SAFETY: the DLL's exports and its objects' tables as COM lays them out: IClassFactory's
    // CreateInstance fourth, the handler's IsMemberOf fourth and GetOverlayInfo fifth.
    unsafe {
        let module = LoadLibraryW(wide(&dll.to_string_lossy()).as_ptr());
        if module.is_null() {
            bail!(
                "cannot load {}: {}",
                dll.display(),
                io::Error::last_os_error()
            );
        }
        let get = GetProcAddress(module, c"DllGetClassObject".as_ptr().cast())
            .ok_or_else(|| anyhow!("{} has no DllGetClassObject", dll.display()))?;
        let get: GetClassObject = std::mem::transmute(get);
        let mut handlers = Vec::new();
        for o in OVERLAYS {
            let mut factory: *mut c_void = std::ptr::null_mut();
            let hr = get(&parse(o.clsid), &IID_ICLASSFACTORY, &mut factory);
            if hr != 0 {
                bail!("no factory for {:?}: {hr:#x}", o.badge);
            }
            let create: CreateInstance =
                std::mem::transmute(*(*(factory as *const *const usize)).add(3));
            let mut handler: *mut c_void = std::ptr::null_mut();
            let hr = create(factory, std::ptr::null_mut(), &IID_HANDLER, &mut handler);
            if hr != 0 {
                bail!("no handler for {:?}: {hr:#x}", o.badge);
            }
            let table = *(handler as *const *const usize);
            let mut icon = [0u16; 1024];
            let (mut index, mut flags) = (0, 0);
            let info: GetOverlayInfo = std::mem::transmute(*table.add(4));
            let hr = info(
                handler,
                icon.as_mut_ptr(),
                icon.len() as i32,
                &mut index,
                &mut flags,
            );
            let end = icon.iter().position(|c| *c == 0).unwrap_or(0);
            let icon = PathBuf::from(String::from_utf16_lossy(&icon[..end]));
            println!(
                "{:?}: icon {} ({}), {hr:#x}",
                o.badge,
                icon.display(),
                if icon.is_file() { "there" } else { "missing" }
            );
            handlers.push((
                o.badge,
                handler,
                std::mem::transmute::<usize, IsMemberOf>(*table.add(3)),
            ));
        }
        let ask = |tries: u32| {
            for path in paths {
                let p = wide(&path.to_string_lossy());
                let mut said = "none".to_string();
                for _ in 0..tries {
                    if let Some((badge, _, _)) = handlers
                        .iter()
                        .find(|(_, h, is)| is(*h, p.as_ptr(), 0) == 0)
                    {
                        said = format!("{badge:?}");
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                println!("{}: {said}", path.display());
            }
        };
        ask(40);
        if let Some(secs) = again {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            println!("{secs} s later:");
            // Once: what the handler holds now, as Explorer would be shown it.
            ask(1);
        }
    }
    Ok(())
}
