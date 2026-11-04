//! A stand-in daemon for trying the tray without one: it serves `cairn_protocol::local` for a scratch
//! root, registers it in the registry, and makes up activity. Point CAIRN_USER_DIR at a scratch
//! directory for it and the tray alike, so the real registry and daemons are not touched.
//!
//!     cargo run --example mock-daemon -- /tmp/mock-root [--problems]

use cairn_protocol::local::{
    control_address, registry_path, Activity, Change, DaemonStatus, Direction, LocalAddress,
    NotApplied, OpenConflict, Registry, Reply, Request, RootEntry, Waiting,
};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ROOT_ID: &str = "00000000000000000000000000mock00";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Shared {
    status: Mutex<(u64, DaemonStatus)>,
    changed: Condvar,
    root: PathBuf,
}

impl Shared {
    fn change(&self, f: impl FnOnce(&mut DaemonStatus)) {
        let mut g = self.status.lock().unwrap();
        f(&mut g.1);
        g.0 += 1;
        self.changed.notify_all();
    }
}

fn copy_name() -> &'static str {
    "notes (conflict mock 2026-11-04).txt"
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(
        args.first()
            .expect("usage: mock-daemon <root dir> [--problems]"),
    );
    let problems = args.iter().any(|a| a == "--problems");
    std::fs::create_dir_all(root.join(".cairn")).unwrap();
    std::fs::write(root.join(".cairn/root-id"), format!("{ROOT_ID}\n")).unwrap();
    for (name, text) in [
        ("notes.txt", "the original\n"),
        (copy_name(), "the competing version\n"),
    ] {
        if !root.join(name).exists() {
            std::fs::write(root.join(name), text).unwrap();
        }
    }
    let root = root.canonicalize().unwrap();
    let reg = registry_path().expect("no registry path");
    std::fs::create_dir_all(reg.parent().unwrap()).unwrap();
    let registry = Registry {
        roots: vec![RootEntry {
            path: root.clone(),
            root_id: ROOT_ID.into(),
            server: "https://example.invalid:11081".into(),
            added: now(),
        }],
    };
    std::fs::write(&reg, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    println!("registry {} names {}", reg.display(), root.display());

    let mut status = DaemonStatus {
        pid: std::process::id(),
        state: "synced".into(),
        last_sync: now(),
        last_synced: now(),
        head: 100,
        syncs: 1,
        watched: 12,
        root: root.clone(),
        root_id: ROOT_ID.into(),
        server: "https://example.invalid:11081".into(),
        version: "mock".into(),
        ..Default::default()
    };
    if problems {
        status.state = "partial".into();
        status.detail = Some("1 change waits".into());
        status.skips = vec![format!(
            "a/CON.txt: {}",
            cairn_protocol::SkipKind::CannotHoldName.reason()
        )];
        status.not_applied = vec![NotApplied {
            path: "b/script.sh".into(),
            text: "mode 0755".into(),
            since: now() - 600,
        }];
        status.not_applied_count = 1;
        status.waiting = Some(Waiting {
            until: now() + 120,
            why: "a file is still being written".into(),
        });
    }
    let shared = Arc::new(Shared {
        status: Mutex::new((1, status)),
        changed: Condvar::new(),
        root,
    });

    let s = shared.clone();
    std::thread::spawn(move || {
        let mut n = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(4));
            n += 1;
            if s.status.lock().unwrap().1.paused_since.is_some() {
                continue;
            }
            s.change(|st| st.syncing = true);
            std::thread::sleep(Duration::from_secs(1));
            s.change(|st| {
                st.syncing = false;
                st.head += 1;
                st.syncs += 1;
                st.last_sync = now();
                st.last_synced = now();
                st.recent.insert(
                    0,
                    Activity {
                        time: now(),
                        direction: if n.is_multiple_of(2) {
                            Direction::Up
                        } else {
                            Direction::Down
                        },
                        change: [
                            Change::Added,
                            Change::Changed,
                            Change::Moved,
                            Change::Removed,
                        ][(n % 4) as usize],
                        path: format!("docs/file-{n}.txt"),
                        to: (n % 4 == 2).then(|| format!("docs/moved-{n}.txt")),
                    },
                );
                st.recent.truncate(cairn_protocol::local::MAX_RECENT);
            });
        }
    });

    serve(control_address(ROOT_ID).expect("no address"), shared);
}

fn answer(stream: impl std::io::Read + Write, shared: &Shared) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            return send(
                reader.get_mut(),
                &Reply::Error {
                    text: format!("not a request: {e}"),
                },
            )
        }
    };
    println!("request: {request:?}");
    let out = reader.get_mut();
    match request {
        Request::Status => {
            let st = shared.status.lock().unwrap().1.clone();
            send(out, &Reply::Status(Box::new(st)));
        }
        Request::Subscribe => {
            let mut seen = 0;
            loop {
                let st = {
                    let g = shared.status.lock().unwrap();
                    let (g, _) = shared
                        .changed
                        .wait_timeout_while(g, Duration::from_secs(30), |g| g.0 == seen)
                        .unwrap();
                    seen = g.0;
                    g.1.clone()
                };
                let mut line = serde_json::to_vec(&Reply::Status(Box::new(st))).unwrap();
                line.push(b'\n');
                if out.write_all(&line).and_then(|()| out.flush()).is_err() {
                    return;
                }
            }
        }
        Request::SyncNow => {
            shared.change(|st| st.last_sync = now());
            send(out, &Reply::Done);
        }
        Request::Pause => {
            shared.change(|st| {
                st.paused_since = Some(now());
                st.state = "paused".into();
            });
            send(out, &Reply::Done);
        }
        Request::Resume => {
            shared.change(|st| {
                st.paused_since = None;
                st.state = "synced".into();
            });
            send(out, &Reply::Done);
        }
        Request::Conflicts => send(
            out,
            &Reply::Conflicts {
                open: conflicts(&shared.root),
            },
        ),
    }
}

/// Open while the copy is there under its conflict name.
fn conflicts(root: &Path) -> Vec<OpenConflict> {
    let copy = root.join(copy_name());
    if !copy.exists() {
        return Vec::new();
    }
    let original = root.join("notes.txt");
    vec![OpenConflict {
        seq: 42,
        time: now() - 3600,
        device_name: "mock-device".into(),
        copy: Some(copy),
        original: original.exists().then_some(original),
        copy_display: copy_name().into(),
        original_display: Some("notes.txt".into()),
    }]
}

fn send(out: &mut impl Write, reply: &Reply) {
    let mut line = serde_json::to_vec(reply).unwrap();
    line.push(b'\n');
    let _ = out.write_all(&line);
    let _ = out.flush();
}

#[cfg(unix)]
fn serve(addr: LocalAddress, shared: Arc<Shared>) {
    use std::os::unix::net::UnixListener;
    let listener = match addr {
        #[cfg(target_os = "linux")]
        LocalAddress::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt;
            let a = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
            UnixListener::bind_addr(&a).unwrap()
        }
        LocalAddress::Path(p) => {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let _ = std::fs::remove_file(&p);
            UnixListener::bind(&p).unwrap()
        }
        other => panic!("cannot serve {other:?}"),
    };
    println!("listening");
    for stream in listener.incoming().flatten() {
        let s = shared.clone();
        std::thread::spawn(move || answer(stream, &s));
    }
}

#[cfg(windows)]
fn serve(addr: LocalAddress, shared: Arc<Shared>) {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let LocalAddress::Pipe(name) = addr else {
        panic!("cannot serve {addr:?}")
    };
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    println!("listening");
    loop {
        // SAFETY: a NUL-terminated name; the default security of the creator's own processes.
        let h = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        };
        assert!(
            h != INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the pipe just made. ERROR_PIPE_CONNECTED (535) means a client came first.
        let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
            || std::io::Error::last_os_error().raw_os_error() == Some(535);
        // SAFETY: the handle is ours to own from here.
        let file = unsafe { std::fs::File::from_raw_handle(h) };
        if ok {
            let s = shared.clone();
            std::thread::spawn(move || answer(file, &s));
        }
    }
}
