//! The COM side: for each badge, a class factory and an icon overlay handler. All are static, as Explorer
//! needs one of each and keeps it as long as it runs, so there is nothing to count or free. Every entry
//! point catches a panic, which would otherwise unwind into Explorer.

use crate::worker;
use cairn_protocol::local::OVERLAYS;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;
use windows_sys::core::{BOOL, GUID, HRESULT, PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOINTERFACE, E_POINTER, HMODULE,
    S_FALSE, S_OK,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::UI::Shell::{ISIOI_ICONFILE, ISIOI_ICONINDEX};

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ICLASSFACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);
const IID_ISHELLICONOVERLAYIDENTIFIER: GUID =
    GUID::from_u128(0x0c6c4200_c589_11d0_999a_00c04fd655e1);

fn same(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

/// A class ID as the registry writes it, `{XXXXXXXX-XXXX-...}`.
fn parse(clsid: &str) -> Option<GUID> {
    let hex: String = clsid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (hex.len() == 32)
        .then(|| u128::from_str_radix(&hex, 16).ok())
        .flatten()
        .map(GUID::from_u128)
}

fn guard(f: impl FnOnce() -> HRESULT) -> HRESULT {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(E_FAIL)
}

type QueryInterface =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type Count = unsafe extern "system" fn(*mut c_void) -> u32;

#[repr(C)]
struct FactoryVtbl {
    query_interface: QueryInterface,
    add_ref: Count,
    release: Count,
    create_instance: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
    lock_server: unsafe extern "system" fn(*mut c_void, BOOL) -> HRESULT,
}

#[repr(C)]
struct OverlayVtbl {
    query_interface: QueryInterface,
    add_ref: Count,
    release: Count,
    is_member_of: unsafe extern "system" fn(*mut c_void, PCWSTR, u32) -> HRESULT,
    get_overlay_info:
        unsafe extern "system" fn(*mut c_void, PWSTR, i32, *mut i32, *mut u32) -> HRESULT,
    get_priority: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
}

/// What a COM pointer points at: the table first, then which of [`OVERLAYS`] it is.
#[repr(C)]
struct Object<V: 'static> {
    vtbl: &'static V,
    which: usize,
}

static FACTORY_VTBL: FactoryVtbl = FactoryVtbl {
    query_interface: factory_query_interface,
    add_ref,
    release,
    create_instance,
    lock_server,
};

static OVERLAY_VTBL: OverlayVtbl = OverlayVtbl {
    query_interface: overlay_query_interface,
    add_ref,
    release,
    is_member_of,
    get_overlay_info,
    get_priority,
};

static FACTORIES: [Object<FactoryVtbl>; 4] = [
    Object {
        vtbl: &FACTORY_VTBL,
        which: 0,
    },
    Object {
        vtbl: &FACTORY_VTBL,
        which: 1,
    },
    Object {
        vtbl: &FACTORY_VTBL,
        which: 2,
    },
    Object {
        vtbl: &FACTORY_VTBL,
        which: 3,
    },
];

static HANDLERS: [Object<OverlayVtbl>; 4] = [
    Object {
        vtbl: &OVERLAY_VTBL,
        which: 0,
    },
    Object {
        vtbl: &OVERLAY_VTBL,
        which: 1,
    },
    Object {
        vtbl: &OVERLAY_VTBL,
        which: 2,
    },
    Object {
        vtbl: &OVERLAY_VTBL,
        which: 3,
    },
];

/// Which of [`OVERLAYS`] an object is.
///
/// # Safety
/// `this` is one of [`FACTORIES`] or [`HANDLERS`], as COM only calls an object through itself.
unsafe fn which<V: 'static>(this: *mut c_void) -> usize {
    (*(this as *const Object<V>)).which.min(OVERLAYS.len() - 1)
}

unsafe fn hand_out(ppv: *mut *mut c_void, object: *const c_void) -> HRESULT {
    *ppv = object as *mut c_void;
    S_OK
}

unsafe extern "system" fn add_ref(_: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn release(_: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn factory_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    if same(&*riid, &IID_IUNKNOWN) || same(&*riid, &IID_ICLASSFACTORY) {
        return hand_out(ppv, this);
    }
    *ppv = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "system" fn create_instance(
    this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let handler = &HANDLERS[which::<FactoryVtbl>(this)] as *const _ as *mut c_void;
    overlay_query_interface(handler, riid, ppv)
}

unsafe extern "system" fn lock_server(_: *mut c_void, _: BOOL) -> HRESULT {
    S_OK
}

unsafe extern "system" fn overlay_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    if same(&*riid, &IID_IUNKNOWN) || same(&*riid, &IID_ISHELLICONOVERLAYIDENTIFIER) {
        return hand_out(ppv, this);
    }
    *ppv = std::ptr::null_mut();
    E_NOINTERFACE
}

/// A terminated wide string as a `String`, unpaired surrogates replaced: no file in a root has those
/// in its name as a path here gives it.
unsafe fn from_wide(p: PCWSTR) -> String {
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

unsafe extern "system" fn is_member_of(this: *mut c_void, path: PCWSTR, _attrib: u32) -> HRESULT {
    if path.is_null() {
        return E_POINTER;
    }
    let badge = OVERLAYS[which::<OverlayVtbl>(this)].badge;
    guard(|| match worker::badge_of(&from_wide(path)) == Some(badge) {
        true => S_OK,
        false => S_FALSE,
    })
}

/// This DLL's folder, where `cairn explorer on` put the icons beside it.
fn folder() -> Option<&'static std::path::Path> {
    static FOLDER: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            static HERE: u8 = 0;
            let mut module: HMODULE = std::ptr::null_mut();
            let mut buf = vec![0u16; 32_768];
            // SAFETY: an address in this DLL, and a buffer of the length given.
            let n = unsafe {
                if GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    &HERE as *const u8 as PCWSTR,
                    &mut module,
                ) == 0
                {
                    return None;
                }
                GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize
            };
            if n == 0 || n >= buf.len() {
                return None;
            }
            let dll = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n]));
            dll.parent().map(|p| p.to_path_buf())
        })
        .as_deref()
}

unsafe extern "system" fn get_overlay_info(
    this: *mut c_void,
    icon_file: PWSTR,
    max: i32,
    index: *mut i32,
    flags: *mut u32,
) -> HRESULT {
    if icon_file.is_null() || index.is_null() || flags.is_null() {
        return E_POINTER;
    }
    let overlay = OVERLAYS[which::<OverlayVtbl>(this)];
    guard(|| {
        let Some(dir) = folder() else {
            return E_FAIL;
        };
        let wide: Vec<u16> = dir
            .join(overlay.icon)
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .collect();
        if wide.len() + 1 > max.max(0) as usize {
            return E_FAIL;
        }
        // SAFETY: room for the name and its terminator, as `max` says the buffer has.
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), icon_file, wide.len());
            *icon_file.add(wide.len()) = 0;
            *index = 0;
            *flags = ISIOI_ICONFILE | ISIOI_ICONINDEX;
        }
        S_OK
    })
}

/// Worst first, though each entry has one badge and so one handler says yes to it.
unsafe extern "system" fn get_priority(this: *mut c_void, priority: *mut i32) -> HRESULT {
    if priority.is_null() {
        return E_POINTER;
    }
    *priority = which::<OverlayVtbl>(this) as i32;
    S_OK
}

/// The factory of one of the handlers, by its class ID.
///
/// # Safety
/// Called by COM, with pointers COM gives.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    let found = OVERLAYS
        .iter()
        .position(|o| parse(o.clsid).is_some_and(|g| same(&g, &*rclsid)));
    match found {
        Some(i) => factory_query_interface(&FACTORIES[i] as *const _ as *mut c_void, riid, ppv),
        None => CLASS_E_CLASSNOTAVAILABLE,
    }
}

/// Never: the threads that watch the daemons run in this DLL's code.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_class_id_reads_as_written() {
        let g = parse("{42E78634-2CE1-4E0F-BF2D-C6126A39756F}").unwrap();
        assert_eq!((g.data1, g.data2, g.data3), (0x42E78634, 0x2CE1, 0x4E0F));
        assert_eq!(g.data4, [0xBF, 0x2D, 0xC6, 0x12, 0x6A, 0x39, 0x75, 0x6F]);
        for o in OVERLAYS {
            assert!(parse(o.clsid).is_some(), "{}", o.clsid);
        }
    }
}
