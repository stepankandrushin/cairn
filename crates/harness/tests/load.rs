//! Loading the state at a sync's start (engine state.rs, `warm`): its file is read through once, in the
//! file's order and in pieces of a MiB, before SQLite walks its tables, which in a state that syncs
//! have rewritten lead all over the file. On a cold cache the disk then reads it at its sequential
//! rate, not a page at a time at its random one: a hard disk's ~150 pages a second made a sync of a
//! big tree sit in "loading" for a quarter of an hour.
//!
//! Linux alone, as only there can one file be dropped from the cache and what of it is cached be asked.

#![cfg(target_os = "linux")]

mod common;
use common::*;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The engine's `WARM_PIECE`.
const PIECE: u64 = 1 << 20;
const FILES: usize = 3_000;

/// Of a file's pages, how many are in the page cache, and how many it has.
fn resident(path: &Path) -> (usize, usize) {
    let file = std::fs::File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    // SAFETY: a query with no arguments.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let pages = len.div_ceil(page);
    if len == 0 {
        return (0, 0);
    }
    let mut vec = vec![0u8; pages];
    // SAFETY: a shared read-only mapping of the whole file, only asked about and unmapped at once.
    unsafe {
        let at = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(at, libc::MAP_FAILED, "map {}", path.display());
        assert_eq!(libc::mincore(at, len, vec.as_mut_ptr()), 0, "mincore");
        libc::munmap(at, len);
    }
    (vec.iter().filter(|b| *b & 1 == 1).count(), pages)
}

/// Out of the page cache, as after a while of other work, or a restart.
fn evict(path: &Path) {
    let file = std::fs::File::open(path).unwrap();
    file.sync_all().unwrap();
    // SAFETY: an advice on a descriptor this function holds.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(rc, 0, "posix_fadvise");
}

/// How often a key-order walk of a table's pages goes anywhere but to the next page of the file, and
/// how many steps it takes: what a load of it costs in seeks on a cold cache, without the warm-up.
fn jumps(state: &Path, table: &str) -> (usize, usize) {
    let db =
        rusqlite::Connection::open_with_flags(state, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = db
        .prepare("SELECT pageno FROM dbstat WHERE name = ?1 ORDER BY path")
        .unwrap();
    let pages: Vec<i64> = stmt
        .query_map([table], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let jumps = pages.windows(2).filter(|w| w[1] != w[0] + 1).count();
    (jumps, pages.len().saturating_sub(1))
}

/// A field of `/proc/<pid>/io`.
fn io_field(pid: u32, field: &str) -> u64 {
    let text = std::fs::read_to_string(format!("/proc/{pid}/io")).unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{field}: ")))
        .unwrap_or_else(|| panic!("no {field} in {text}"))
        .parse()
        .unwrap()
}

/// A state of 3,000 files rewritten three times, so that its tables are out of the file's order and it
/// keeps free pages, and then out of the cache: by the time the sync opens it with SQLite, all of it is
/// in the cache again, brought there in reads of a MiB, and the sync goes on to agree as before.
#[test]
fn a_sync_reads_its_state_through_in_order_before_it_loads_it() {
    let sb = Sandbox::on_disk("load-warm");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    for i in 0..FILES {
        a.write(&format!("d{:02}/f{i:05}", i % 50), format!("file {i}"));
    }
    a.synced();
    // Each round moves the times of a different half, whose rows, base nodes and ledger entries are
    // written again where the free pages are.
    for round in 0..3 {
        for i in (round % 2..FILES).step_by(2) {
            let sec = 1_800_000_000 + (round * FILES + i) as i64;
            set_mtime(&a.path(&format!("d{:02}/f{i:05}", i % 50)), sec, 0);
        }
        a.synced();
    }
    let state = a.path(".cairn/state.sqlite");
    let (out_of_order, steps) = jumps(&state, "rows");
    assert!(
        out_of_order * 4 > steps,
        "a walk of the rows jumps {out_of_order} times in {steps} steps: in the file's order, which proves nothing"
    );
    evict(&state);
    let (cached, pages) = resident(&state);
    if cached * 10 > pages {
        skip(&format!(
            "{cached} of the state's {pages} pages stay cached when told to go: this filesystem keeps them, as tmpfs does"
        ));
        return;
    }

    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let mut sync = Command::new(CAIRN)
        .args(["--root", a.root.to_str().unwrap(), "sync"])
        .env("CAIRN_FP_PAUSE", format!("root.warmed:{}", gate.display()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !gate.join("reached").exists() {
        assert!(Instant::now() < deadline, "the sync never opened its state");
        assert!(
            sync.try_wait().unwrap().is_none(),
            "the sync ended before it opened its state"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let (cached, pages) = resident(&state);
    let (calls, bytes) = (io_field(sync.id(), "syscr"), io_field(sync.id(), "rchar"));
    std::fs::write(gate.join("go"), b"").unwrap();
    let out = sync.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let len = std::fs::metadata(&state).unwrap().len();
    eprintln!(
        "rows' walk jumps {out_of_order} times in {steps}; as SQLite opened the state, {cached} of its {pages} pages were cached, after {calls} reads of {bytes} bytes for its {len}"
    );
    assert!(
        cached * 100 >= pages * 95,
        "{cached} of the state's {pages} pages were cached as SQLite opened it: the load read the rest where its tables led"
    );
    assert!(
        bytes >= len,
        "{bytes} bytes read before the state was opened, of its {len}"
    );
    // The pieces, the end of the file, and the few small files read before.
    let most = len.div_ceil(PIECE) + 1 + 64;
    assert!(
        calls <= most,
        "{calls} reads before the state was opened, for {len} bytes: more than {most}, so not in pieces of a MiB"
    );
    a.synced();
}
