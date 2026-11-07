//! A file the system refuses this device, to read or to write, as an access list, another program or
//! one that guards files does: reported by name in the system's own words, held as it was, tried again,
//! and never sent as a delete or a change. A tree mostly refused is not synced at all until it can be
//! read. A file that is gone, whatever took it, is a delete as before, and listed to bring back.
mod common;
use common::*;
use std::fs;
use std::path::{Path, PathBuf};

/// EACCES; on Windows ERROR_VIRUS_INFECTED, what Windows gives for a file a security program blocks.
const REFUSED: &str = if cfg!(windows) { "225" } else { "13" };

/// The system's words for it, as the report carries them.
const SAID: &str = if cfg!(windows) {
    "os error 225"
} else {
    "os error 13"
};

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// A gate: while its file is there, the point refuses that entry.
fn gate(sb: &Sandbox, point: &str, entry: &str) -> (PathBuf, String) {
    let file = sb.path(&format!("gate-{:x}", rand::random::<u32>()));
    fs::write(&file, b"").unwrap();
    let spec = format!("{point}:{entry}:{REFUSED}:{}", file.display());
    (file, spec)
}

#[track_caller]
fn says_refused(run: &Run, path: &str) {
    let line = run
        .stdout
        .lines()
        .find(|l| l.starts_with(&format!("skipped: {path}: waiting for permission")))
        .unwrap_or_else(|| panic!("no refusal of {path}: {}", run.text()));
    assert!(line.contains(SAID), "not in the system's words: {line}");
    if cfg!(windows) {
        assert!(line.contains("security software"), "{line}");
    }
}

fn open(file: &Path) {
    fs::remove_file(file).unwrap();
}

/// A changed file whose read is refused mid-scan stays as it was everywhere: not a delete, not a
/// revision. Everything else goes on, and once it can be read its change goes up.
#[test]
fn a_read_refused_mid_scan_is_held_reported_and_never_a_delete() {
    let (sb, _s, a, b) = pair("refused-read");
    a.write("f", "v1");
    a.write("keep", "k");
    a.synced();
    b.synced();

    a.write("f", "v2, written here");
    a.write("new", "n");
    let (lock, spec) = gate(&sb, "scan.open", "f");
    let run = a.sync_env(&[("CAIRN_FP_REFUSE", &spec)]);
    assert_eq!(run.code, Some(3), "{}", run.text());
    says_refused(&run, "f");
    // Refused again on the next sync, and still said.
    let run = a.sync_env(&[("CAIRN_FP_REFUSE", &spec)]);
    assert_eq!(run.code, Some(3), "{}", run.text());
    says_refused(&run, "f");

    b.synced();
    assert_eq!(b.read("f"), b"v1", "another device lost it or got a change");
    assert_eq!(b.read("new"), b"n", "the rest was held back");

    open(&lock);
    converge(&[&a, &b]);
    assert_eq!(b.read("f"), b"v2, written here");
}

/// A write refused on apply, for a file that changed and for a new one: what is here stays as it was,
/// the sync says so, nothing goes back as a delete or a revert, and the next sync once allowed takes
/// both.
#[test]
fn a_write_refused_on_apply_is_held_and_tried_again() {
    let (sb, _s, a, b) = pair("refused-write");
    a.write("f", "v1");
    a.write("keep", "k");
    a.synced();
    b.synced();

    a.write("f", "v2 from a");
    a.write("g", "new from a");
    a.synced();
    let (lock, spec) = gate(&sb, "apply.place", "f");
    let run = b.sync_env(&[("CAIRN_FP_REFUSE", &spec)]);
    assert_eq!(run.code, Some(3), "{}", run.text());
    says_refused(&run, "f");
    assert_eq!(b.read("f"), b"v1");
    assert_eq!(b.read("g"), b"new from a", "the rest was held back");
    let run = b.sync_env(&[("CAIRN_FP_REFUSE", &spec)]);
    assert_eq!(run.code, Some(3), "{}", run.text());
    says_refused(&run, "f");
    open(&lock);

    // A new file refused, after an edit there too.
    a.write("h", "another new one");
    a.synced();
    let (lock, spec) = gate(&sb, "apply.place", "h");
    let run = b.sync_env(&[("CAIRN_FP_REFUSE", &spec)]);
    assert_eq!(run.code, Some(3), "{}", run.text());
    says_refused(&run, "h");
    assert!(!b.exists("h"));
    assert_eq!(b.read("f"), b"v2 from a", "allowed again, it came");

    // b sent nothing for any of it: a has every file as it made it.
    a.synced();
    assert_eq!(a.read("f"), b"v2 from a");
    assert_eq!(a.read("h"), b"another new one");

    open(&lock);
    converge(&[&a, &b]);
    assert_eq!(b.read("h"), b"another new one");
}

/// Most of a tree refused, as a disk failing or a folder closed to this user makes it, is not synced
/// at all: paused, with every file listed, nothing sent and nothing changed, and it goes on by itself
/// once the files can be read.
#[test]
fn a_tree_mostly_refused_pauses_and_sends_nothing() {
    let (sb, _s, a, b) = pair("refused-most");
    for i in 0..12 {
        a.write(&format!("d/{i:02}"), format!("v1 {i}"));
    }
    a.write("keep", "k");
    a.synced();
    b.synced();

    for i in 0..12 {
        a.write(&format!("d/{i:02}"), format!("v2 {i}"));
    }
    a.write("new", "n");
    let (lock, spec) = gate(&sb, "scan.open", "*");
    let run = a.sync_env(&[("CAIRN_FP_REFUSE", &spec)]);
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(
        run.stdout
            .contains("cannot be read: the system refuses them"),
        "{}",
        run.text()
    );
    says_refused(&run, "d/00");
    // `--allow-mass-delete` is for what vanished, not for what the system refuses.
    let run = a.cairn_env(
        &["sync", "--allow-mass-delete"],
        &[("CAIRN_FP_REFUSE", &spec)],
    );
    assert_eq!(run.code, Some(2), "{}", run.text());

    b.synced();
    for i in 0..12 {
        assert_eq!(b.read(&format!("d/{i:02}")), format!("v1 {i}").as_bytes());
    }
    assert!(!b.exists("new"), "a paused sync sent something");

    open(&lock);
    a.synced();
    b.synced();
    assert_eq!(b.read("d/11"), b"v2 11");
    assert_eq!(b.read("new"), b"n");
}

/// Where the system itself refuses a folder, by its mode, the same holds without a failpoint.
#[cfg(unix)]
#[test]
fn a_folder_closed_to_this_user_pauses_when_it_is_most_of_the_tree() {
    if skip_as_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("refused-mode");
    for i in 0..12 {
        a.write(&format!("d/{i:02}"), format!("{i}"));
    }
    a.write("keep", "k");
    a.synced();
    b.synced();
    chmod(&a.path("d"), 0o000);
    let run = a.sync();
    chmod(&a.path("d"), 0o755);
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(
        run.stdout
            .contains("12 of 14 synced entries here cannot be read"),
        "{}",
        run.text()
    );
    b.synced();
    assert!(b.exists("d/11"));
    a.synced();
}

/// A file that arrives and is then taken away here, as an antivirus quarantines one: gone, so a delete
/// like any other, for every device. Its record says who and when, and it comes back from the list.
/// What is taken before its arrival is recorded comes down again instead (interleavings.rs).
#[test]
fn a_file_taken_after_it_arrived_is_a_delete_listed_to_bring_back() {
    let (_sb, _s, a, b) = pair("refused-taken");
    a.write("keep", "k");
    a.synced();
    b.synced();
    a.write("tool.exe", "flagged here");
    a.synced();
    b.synced();
    assert_eq!(b.read("tool.exe"), b"flagged here");

    fs::remove_file(b.path("tool.exe")).unwrap();
    b.synced();
    a.synced();
    assert!(!a.exists("tool.exe"));

    let run = a.cairn(&["deleted", "--json"]).expect_ok();
    let entry: serde_json::Value = run
        .stdout
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["path"] == "tool.exe")
        .unwrap_or_else(|| panic!("not listed: {}", run.text()));
    assert_eq!(entry["device_name"], "b", "the device it went from");
    let id = entry["id"].as_str().unwrap();
    a.cairn(&["undelete", id]).expect_ok();
    b.synced();
    assert_eq!(b.read("tool.exe"), b"flagged here");
}

/// The antivirus test file, reversed so that this source is no such file to anything that scans it.
#[cfg(windows)]
fn eicar() -> Vec<u8> {
    let reversed = br"*H+H$!ELIF-TSET-SURIVITNA-DRADNATS-RACIE$}7)CC7)^P(45XZP\4[PA@%P!O5X";
    reversed.iter().rev().copied().collect()
}

/// What this machine's antivirus really does, run by hand where one is on: Windows' own errors for a
/// file it blocks or takes, written and read by a program it does not trust, and then the same file
/// arriving through a sync. `CAIRN_AV_SAFE_DIR` is a folder the antivirus leaves alone, for the server
/// and the device that sends it; the device that receives it is in this build's folder, which it scans.
/// Whatever it does, no device may lose the file without a delete that says which device made it.
#[cfg(windows)]
#[test]
#[ignore]
fn an_antivirus_taking_a_file_as_it_arrives() {
    use std::io::Read;
    let Some(safe) = std::env::var_os("CAIRN_AV_SAFE_DIR") else {
        panic!("set CAIRN_AV_SAFE_DIR to a folder the antivirus leaves alone");
    };
    let code = |e: &std::io::Error| format!("{e} (raw {:?})", e.raw_os_error());

    // A program writing it, and reading it back, where it is scanned.
    let plain = Sandbox::on_disk("av-plain");
    let file = plain.path("test.txt");
    match fs::write(&file, eicar()) {
        Ok(()) => eprintln!("write: ok"),
        Err(e) => eprintln!("write: {}", code(&e)),
    }
    for wait in [0, 3] {
        std::thread::sleep(std::time::Duration::from_secs(wait));
        let read = fs::File::open(&file).and_then(|mut f| {
            let mut v = Vec::new();
            f.read_to_end(&mut v).map(|_| v.len())
        });
        match read {
            Ok(n) => eprintln!("read after {wait} s: {n} bytes"),
            Err(e) => eprintln!("read after {wait} s: {}", code(&e)),
        }
        eprintln!("there after {wait} s: {}", file.exists());
    }

    // The same through a sync.
    let sb = Sandbox::under(Path::new(&safe), "av-safe");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    a.write("keep", "k");
    a.write("test.txt", eicar());
    a.synced();
    let there = Sandbox::on_disk("av-there");
    let b = Device::new(&there, &server, "b");
    let first = b.sync();
    eprintln!("b's first sync, exit {:?}:\n{}", first.code, first.text());
    std::thread::sleep(std::time::Duration::from_secs(5));
    eprintln!("on b after 5 s: {}", b.exists("test.txt"));
    let again = b.sync();
    eprintln!("b's second sync, exit {:?}:\n{}", again.code, again.text());
    // Then the antivirus takes it, in its own time, and b syncs once it has.
    let since = std::time::Instant::now();
    while b.exists("test.txt") && since.elapsed() < std::time::Duration::from_secs(120) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!(
        "on b after {:.0} s: {}",
        since.elapsed().as_secs_f64(),
        b.exists("test.txt")
    );
    for n in 3..5 {
        let run = b.sync();
        eprintln!("b's sync {n}, exit {:?}:\n{}", run.code, run.text());
    }
    let on_a = a.sync();
    eprintln!("a's sync after, exit {:?}:\n{}", on_a.code, on_a.text());
    eprintln!("on a: {}", a.exists("test.txt"));
    let listed = a.cairn(&["deleted", "--json"]);
    eprintln!("a's deleted list:\n{}", listed.text());
    if !a.exists("test.txt") {
        assert!(
            listed.stdout.contains("\"device_name\":\"b\"") && listed.stdout.contains("test.txt"),
            "a lost it with no delete from b to bring back: {}",
            listed.text()
        );
    }
}
