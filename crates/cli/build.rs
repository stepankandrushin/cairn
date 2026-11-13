//! On Windows, cairn.exe carries the daemon's icon and a version resource, so that Task Manager and
//! Explorer show it as "Cairn" with its icon, as macOS shows the daemon, rather than a bare
//! cairn.exe. They are written here as a .res file, the format a resource compiler makes, which the
//! MSVC linker takes as an input: no resource compiler and no crate. On other targets, nothing.

use std::env;
use std::fs;
use std::path::PathBuf;

/// The daemon's icon, drawn by tray/icons/make.py beside the tray's.
const ICON: &str = "../../tray/icons/sync.ico";

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
/// Language neutral, as the tray's resources are; its version strings are in table 000004b0.
const LANGUAGE: u16 = 0;
const CODEPAGE: u16 = 1200;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:warning=cairn.exe is built without its icon and version: only the MSVC linker takes the .res made here");
        return;
    }
    let dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico = fs::read(dir.join(ICON)).unwrap_or_else(|e| panic!("reading {ICON}: {e}"));
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let res = resources(&ico, &version);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("cairn.res");
    fs::write(&out, res).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
    println!("cargo:rustc-link-arg-bin=cairn={}", out.display());
}

/// The .res file: an empty first entry, as every one starts, then each image of the icon, the icon
/// group naming them, and the version.
fn resources(ico: &[u8], version: &str) -> Vec<u8> {
    let mut res = Vec::new();
    entry(&mut res, 0, 0, 0, 0, &[]);
    let images = icon_images(ico);
    let mut group = Vec::new();
    for v in [0u16, 1, images.len() as u16] {
        group.extend(v.to_le_bytes());
    }
    for (i, (head, data)) in images.iter().enumerate() {
        let id = i as u16 + 1;
        entry(&mut res, RT_ICON, id, LANGUAGE, 0x1010, data);
        // The directory entry as in the .ico, less its offset, plus the image's ID.
        group.extend(&head[..12]);
        group.extend(id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, 1, LANGUAGE, 0x1030, &group);
    entry(
        &mut res,
        RT_VERSION,
        1,
        LANGUAGE,
        0x0030,
        &version_info(version),
    );
    res
}

/// One resource: its header (type and name as numbers), then its data, padded to four bytes.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, language: u16, flags: u16, data: &[u8]) {
    res.extend((data.len() as u32).to_le_bytes());
    res.extend(32u32.to_le_bytes());
    for half in [0xFFFF, kind, 0xFFFF, name] {
        res.extend(half.to_le_bytes());
    }
    res.extend(0u32.to_le_bytes());
    res.extend(flags.to_le_bytes());
    res.extend(language.to_le_bytes());
    res.extend(0u32.to_le_bytes());
    res.extend(0u32.to_le_bytes());
    res.extend(data);
    pad(res);
}

/// Each image of an .ico: its 16-byte directory entry and its bytes.
fn icon_images(ico: &[u8]) -> Vec<([u8; 16], &[u8])> {
    let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([ico[i], ico[i + 1], ico[i + 2], ico[i + 3]]) as usize;
    assert!(
        ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1,
        "{ICON} is not an icon file"
    );
    (0..u16_at(4) as usize)
        .map(|i| {
            let at = 6 + 16 * i;
            let head: [u8; 16] = ico[at..at + 16]
                .try_into()
                .expect("an icon's directory entry");
            let (size, offset) = (u32_at(at + 8), u32_at(at + 12));
            (head, &ico[offset..offset + size])
        })
        .collect()
}

/// VS_VERSIONINFO: the fixed part, from the crate's version, and the strings Explorer and Task
/// Manager read, FileDescription first among them.
fn version_info(version: &str) -> Vec<u8> {
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    let high = (u32::from(major) << 16) | u32::from(minor);
    let low = u32::from(patch) << 16;
    // VS_FIXEDFILEINFO: its signature and version, the file's version and the product's (each high and
    // low), the flags that are valid and those set (none), the OS (VOS_NT_WINDOWS32), the type (VFT_APP),
    // no subtype and no date.
    let fixed: Vec<u8> = [
        0xFEEF04BD, 0x00010000, high, low, high, low, 0x3F, 0, 0x00040004, 1, 0, 0, 0,
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "cairn"),
        ("FileDescription", "Cairn"),
        ("FileVersion", version),
        ("InternalName", "cairn"),
        ("OriginalFilename", "cairn.exe"),
        ("ProductName", "Cairn"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(key, value)| {
        let text = utf16(value);
        node(key, &text, (text.len() / 2) as u16, 1, &[])
    })
    .collect();
    let table = node(
        &format!("{LANGUAGE:04x}{CODEPAGE:04x}"),
        &[],
        0,
        1,
        &strings,
    );
    let string_info = node("StringFileInfo", &[], 0, 1, &[table]);
    let mut translation = LANGUAGE.to_le_bytes().to_vec();
    translation.extend(CODEPAGE.to_le_bytes());
    let var = node("Translation", &translation, 4, 0, &[]);
    let var_info = node("VarFileInfo", &[], 0, 1, &[var]);
    node(
        "VS_VERSION_INFO",
        &fixed,
        fixed.len() as u16,
        0,
        &[string_info, var_info],
    )
}

/// One block of the version resource: its length, its value's length (in characters for text), its
/// type (1 text, 0 binary), its key, its value, and its children, each on a four-byte boundary.
fn node(key: &str, value: &[u8], value_length: u16, kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut block = vec![0, 0];
    block.extend(value_length.to_le_bytes());
    block.extend(kind.to_le_bytes());
    block.extend(utf16(key));
    pad(&mut block);
    block.extend(value);
    for child in children {
        pad(&mut block);
        block.extend(child);
    }
    let length = block.len() as u16;
    block[..2].copy_from_slice(&length.to_le_bytes());
    block
}

/// UTF-16 little-endian, with its terminating zero.
fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn pad(bytes: &mut Vec<u8>) {
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}
