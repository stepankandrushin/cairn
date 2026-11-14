//! What a photo or video is called on the server.
//!
//! By default the name Nextcloud's iOS app gave, so that a folder its uploads went to goes on as it
//! was: `yy-MM-dd HH-mm-ss NNNN.ext`, the time the photo was taken where it was taken, `NNNN` the four
//! characters after a camera's `IMG_` (`IMG_0003.HEIC` gives `0003`) or else the name's stem whole, and
//! the extension in lower case. A Live Photo's video, `IMG_0003.MOV`, so lands beside its photo with the same stem.

/// How an upload is named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NameStyle {
    /// `yy-MM-dd HH-mm-ss NNNN.ext`, as Nextcloud's iOS app named its uploads.
    Nextcloud,
    /// The name the photo has on the phone, `IMG_0003.HEIC`.
    Original,
}

/// The civil date and time of `sec` at `offset` seconds east of UTC.
pub(crate) fn civil(sec: i64, offset: i32) -> (i64, u32, u32, u32, u32, u32) {
    let t = sec + i64::from(offset);
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400) as u32;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

/// The name a resource gets, before any clash is numbered.
pub fn file_name(style: NameStyle, original: &str, created_sec: i64, offset: i32) -> String {
    let original = original.replace(['/', '\0'], "_");
    let original = if original.is_empty() || original == "." || original == ".." {
        "photo".to_string()
    } else {
        original
    };
    match style {
        NameStyle::Original => original,
        NameStyle::Nextcloud => {
            let (y, mo, d, h, mi, s) = civil(created_sec, offset);
            let (stem, ext) = match original.rfind('.') {
                Some(i) if i > 0 => (&original[..i], Some(original[i + 1..].to_lowercase())),
                _ => (original.as_str(), None),
            };
            let chars: Vec<char> = stem.chars().collect();
            // Nextcloud took the four characters after a camera's `IMG_`, `DSC_` or the like; a name
            // of another kind is kept whole.
            let number: String = if chars.len() >= 8 && chars[3] == '_' {
                chars[4..8].iter().collect()
            } else {
                stem.to_string()
            };
            let mut name = format!(
                "{:02}-{mo:02}-{d:02} {h:02}-{mi:02}-{s:02} {number}",
                y.rem_euclid(100)
            );
            if let Some(ext) = ext.filter(|e| !e.is_empty()) {
                name.push('.');
                name.push_str(&ext);
            }
            name
        }
    }
}

/// Where in the folder it goes: `base`, then `YYYY/MM` if `subfolders`, then its name.
pub fn dest_path(
    base: &str,
    subfolders: bool,
    name: &str,
    created_sec: i64,
    offset: i32,
) -> String {
    let mut parts: Vec<String> = base
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .map(str::to_string)
        .collect();
    if subfolders {
        let (y, m, ..) = civil(created_sec, offset);
        parts.push(format!("{y:04}"));
        parts.push(format!("{m:02}"));
    }
    parts.push(name.to_string());
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time_is_right_on_known_days() {
        assert_eq!(civil(0, 0), (1970, 1, 1, 0, 0, 0));
        // 2023-08-13 16:50:14 at +07:00.
        assert_eq!(civil(1_691_920_214, 7 * 3600), (2023, 8, 13, 16, 50, 14));
        assert_eq!(civil(951_782_400, 0), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil(-1, 0), (1969, 12, 31, 23, 59, 59));
    }

    #[test]
    fn nextcloud_names_match_the_old_folder() {
        // As in the folder Nextcloud's iOS app filled: `23-08-13 16-50-14 0003.jpg` and its `.mov`.
        let t = 1_691_920_214;
        let off = 7 * 3600;
        assert_eq!(
            file_name(NameStyle::Nextcloud, "IMG_0003.HEIC", t, off),
            "23-08-13 16-50-14 0003.heic"
        );
        assert_eq!(
            file_name(NameStyle::Nextcloud, "IMG_0003.MOV", t, off),
            "23-08-13 16-50-14 0003.mov"
        );
        assert_eq!(
            file_name(NameStyle::Nextcloud, "IMG_8B3C1234.JPG", t, off),
            "23-08-13 16-50-14 8B3C.jpg"
        );
        assert_eq!(
            file_name(NameStyle::Nextcloud, "a.png", t, off),
            "23-08-13 16-50-14 a.png"
        );
        assert_eq!(
            file_name(NameStyle::Nextcloud, "Tree.heic", t, off),
            "23-08-13 16-50-14 Tree.heic"
        );
        assert_eq!(
            file_name(NameStyle::Nextcloud, "iMac Blue.heic", t, off),
            "23-08-13 16-50-14 iMac Blue.heic"
        );
        assert_eq!(
            file_name(NameStyle::Nextcloud, "DSC_1234.ARW", t, off),
            "23-08-13 16-50-14 1234.arw"
        );
        assert_eq!(
            file_name(NameStyle::Original, "IMG_0003.HEIC", t, off),
            "IMG_0003.HEIC"
        );
        assert_eq!(file_name(NameStyle::Original, "a/b", t, off), "a_b");
    }

    #[test]
    fn paths_keep_to_names() {
        assert_eq!(
            dest_path("/Photos//iPhone/", false, "x.heic", 0, 0),
            "Photos/iPhone/x.heic"
        );
        assert_eq!(
            dest_path("Photos/../iPhone", true, "x.heic", 1_691_920_214, 0),
            "Photos/iPhone/2023/08/x.heic"
        );
        assert_eq!(dest_path("", false, "x", 0, 0), "x");
    }
}
