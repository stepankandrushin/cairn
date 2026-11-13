//! The local API's two ends (`cairn_protocol::local`): what a daemon serves on its socket, and what
//! `cairn status`, `cairn pause` and the tray ask it with. One JSON object per line each way.
//!
//! Serving never holds up the daemon's syncs: each program has a thread of its own, a status goes to a
//! subscriber through a short queue that is never waited on, and a program that stops reading is
//! dropped, by its queue filling or by a write that does not finish within [`IO_WAIT`].

use cairn_protocol::local::{control_address, DaemonStatus, LocalAddress, Reply, Request};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// For a program to send its request, and for each status to be taken by a subscriber.
pub const IO_WAIT: Duration = Duration::from_secs(5);
/// For `cairn status` to be answered before it reads the daemon's file instead: a daemon from before
/// the local API holds the same socket and never answers.
pub const ASK_WAIT: Duration = Duration::from_secs(2);
/// Statuses waiting for one subscriber. One that falls this far behind is dropped.
const QUEUE: usize = 16;
/// The status again when nothing has changed, so a program that has gone is found and let go.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Each Explorer, and each program showing a file dialog, keeps one watching badges for every root it
/// has shown, beside the tray's and every `cairn status`.
const MAX_CONNECTIONS: usize = 64;
const MAX_REQUEST: usize = 64 * 1024;

pub const DROPPED: &str =
    "a program watching this daemon stopped reading its status, so it was dropped";

pub type Handler = dyn Fn(Request) -> Reply + Send + Sync;

/// The daemon's side: its latest status, and who is subscribed to it.
pub struct Hub {
    line: Mutex<Arc<str>>,
    subscribers: Mutex<Vec<SyncSender<Arc<str>>>>,
    notes: Mutex<Vec<String>>,
    wake: Box<dyn Fn() + Send + Sync>,
    connections: AtomicUsize,
    /// The badges of the root's entries, for a file manager, once the daemon has them.
    board: std::sync::OnceLock<Arc<crate::badges::Board>>,
}

impl Hub {
    /// `wake` brings the daemon round to [`Hub::take_notes`].
    pub fn new(status: &DaemonStatus, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            line: Mutex::new(status_line(status)),
            subscribers: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            wake: Box::new(wake),
            connections: AtomicUsize::new(0),
            board: std::sync::OnceLock::new(),
        }
    }

    /// Serve the badges of this board: [`Request::Badges`] and [`Request::WatchBadges`].
    pub fn serve_badges(&self, board: Arc<crate::badges::Board>) {
        let _ = self.board.set(board);
    }

    pub fn board(&self) -> Option<&Arc<crate::badges::Board>> {
        self.board.get()
    }

    /// The new status, to every subscriber that keeps up. Never waits.
    pub fn publish(&self, status: &DaemonStatus) {
        let line = status_line(status);
        let mut cur = self.line.lock().unwrap_or_else(|e| e.into_inner());
        *cur = line.clone();
        let mut dropped = false;
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| match s.try_send(line.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    dropped = true;
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
        drop(cur);
        if dropped {
            self.lock_notes().push(DROPPED.into());
        }
    }

    /// What the serving threads have to say, for the daemon's log.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock_notes())
    }

    fn lock_notes(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.notes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A line for the daemon's log, which it writes on its next round.
    pub fn note(&self, text: String) {
        self.lock_notes().push(text);
        (self.wake)();
    }

    fn current(&self) -> Arc<str> {
        self.line.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The status now, and every one after it: under the same lock as publishing, so none is missed.
    fn subscribe(&self) -> (Arc<str>, Receiver<Arc<str>>) {
        let cur = self.line.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = sync_channel(QUEUE);
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        (cur.clone(), rx)
    }
}

fn status_line(status: &DaemonStatus) -> Arc<str> {
    reply_line(&Reply::Status(Box::new(status.clone()))).into()
}

fn reply_line(reply: &Reply) -> String {
    let mut line = serde_json::to_string(reply).unwrap_or_else(|e| {
        serde_json::to_string(&Reply::Error {
            text: format!("the reply could not be written: {e}"),
        })
        .expect("a plain error serializes")
    });
    line.push('\n');
    line
}

/// One end of a connection, either side.
trait Conn: Read + Write + Send + 'static {
    fn set_wait(&mut self, wait: Option<Duration>) -> io::Result<()>;
}

/// Serve this daemon's connections, each on a thread of its own. `listener` is what [`listen`] gave.
pub fn serve(listener: Listener, hub: Arc<Hub>, handler: Arc<Handler>) {
    std::thread::spawn(move || {
        if let Err(e) = listener.run(&hub, &handler) {
            hub.note(format!(
                "the local API stopped, so `cairn status` reads this daemon's file and the tray does not see it: {e}"
            ));
        }
    });
}

fn spawn_conn(c: impl Conn, hub: &Arc<Hub>, handler: &Arc<Handler>) {
    if hub.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
        hub.connections.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    let (hub, handler) = (hub.clone(), handler.clone());
    std::thread::spawn(move || {
        // Whatever went wrong was between this daemon and that program: the connection ends.
        let _ = serve_conn(c, &hub, &*handler);
        hub.connections.fetch_sub(1, Ordering::SeqCst);
    });
}

fn serve_conn(mut c: impl Conn, hub: &Hub, handler: &Handler) -> io::Result<()> {
    c.set_wait(Some(IO_WAIT))?;
    let request = match read_line(&mut c)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let request: Request = match serde_json::from_slice(&request) {
        Ok(r) => r,
        Err(e) => {
            let text = format!("not a request of this daemon's: {e}");
            return c.write_all(reply_line(&Reply::Error { text }).as_bytes());
        }
    };
    match request {
        Request::Status => c.write_all(hub.current().as_bytes()),
        Request::Badges { folder } => match hub.board.get() {
            Some(board) => c.write_all(reply_line(&board.badges(&folder)).as_bytes()),
            None => c.write_all(reply_line(&no_badges()).as_bytes()),
        },
        Request::WatchBadges => {
            let Some(board) = hub.board.get() else {
                return c.write_all(reply_line(&no_badges()).as_bytes());
            };
            let (first, rx) = board.watch();
            // Nothing changed, said when nothing has for a while, so a program that has gone is let go.
            let quiet: Arc<str> = reply_line(&Reply::BadgesChanged {
                all: false,
                paths: Vec::new(),
            })
            .into();
            let mut line = first;
            loop {
                c.write_all(line.as_bytes())?;
                line = match rx.recv_timeout(HEARTBEAT) {
                    Ok(line) => line,
                    Err(RecvTimeoutError::Timeout) => quiet.clone(),
                    // Dropped for falling behind: its next connection starts with everything.
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                };
            }
        }
        Request::Subscribe => {
            let (first, rx) = hub.subscribe();
            let mut line = first;
            loop {
                if let Err(e) = c.write_all(line.as_bytes()) {
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) {
                        hub.note(DROPPED.into());
                    }
                    return Err(e);
                }
                line = match rx.recv_timeout(HEARTBEAT) {
                    Ok(line) => line,
                    Err(RecvTimeoutError::Timeout) => hub.current(),
                    // Dropped by the hub for falling behind, which said so.
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                };
            }
        }
        other => c.write_all(reply_line(&handler(other)).as_bytes()),
    }
}

fn no_badges() -> Reply {
    Reply::Error {
        text: "this daemon shows no badges yet".into(),
    }
}

/// One line, without its newline; `None` if the other end closed first.
fn read_line(c: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = c.read(&mut buf)?;
        if n == 0 {
            return Ok((!line.is_empty()).then_some(line));
        }
        if let Some(at) = buf[..n].iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&buf[..at]);
            return Ok(Some(line));
        }
        line.extend_from_slice(&buf[..n]);
        if line.len() > MAX_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a line too long",
            ));
        }
    }
}

/// A connection to a daemon, from `cairn` or the tray.
pub struct Client {
    conn: Box<dyn ClientConn>,
    pending: Vec<u8>,
    /// As last set: macOS refuses to set it again once the other end has closed (EINVAL), which is
    /// how the end of every reply looks.
    wait: Option<Option<Duration>>,
}

trait ClientConn: Read + Write + Send {
    fn wait(&mut self, wait: Option<Duration>) -> io::Result<()>;
}

impl<T: Conn> ClientConn for T {
    fn wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.set_wait(wait)
    }
}

impl Client {
    /// To the daemon of the root with this ID, and only if it runs as this user.
    pub fn connect(root_id: &str) -> io::Result<Self> {
        let address = control_address(root_id)
            .ok_or_else(|| io::Error::other("no local API on this platform"))?;
        Ok(Self {
            conn: connect(&address)?,
            pending: Vec::new(),
            wait: None,
        })
    }

    /// How long a read or a write may take, from now on (`None`: as long as it takes). Set it before
    /// sending: a daemon that answers at once closes at once, and the end that closed cannot be set.
    pub fn set_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        if self.wait != Some(wait) {
            self.conn.wait(wait)?;
            self.wait = Some(wait);
        }
        Ok(())
    }

    pub fn send(&mut self, request: &Request) -> io::Result<()> {
        if self.wait.is_none() {
            self.set_wait(Some(IO_WAIT))?;
        }
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.conn.write_all(&line)
    }

    /// The next reply, waiting at most `wait` (`None`: as long as it takes); `None` once the daemon has
    /// closed.
    pub fn next(&mut self, wait: Option<Duration>) -> io::Result<Option<Reply>> {
        self.set_wait(wait)?;
        loop {
            if let Some(at) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=at).collect();
                return serde_json::from_slice(&line[..at])
                    .map(Some)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            let mut buf = [0u8; 8192];
            let n = self.conn.read(&mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            self.pending.extend_from_slice(&buf[..n]);
        }
    }
}

/// One request and its one reply.
pub fn ask(root_id: &str, request: &Request, wait: Duration) -> io::Result<Reply> {
    let mut c = Client::connect(root_id)?;
    c.set_wait(Some(wait))?;
    c.send(request)?;
    c.next(Some(wait))?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the daemon closed without answering",
        )
    })
}

/// What the daemon of this root says, if one runs: asked on its socket, or else read from its file, as
/// a daemon from before the local API leaves it.
pub fn status_of(path: &Path, root_id: &str) -> Option<DaemonStatus> {
    if let Ok(Reply::Status(s)) = ask(root_id, &Request::Status, ASK_WAIT) {
        return Some(*s);
    }
    crate::daemon::running_status(path)
}

// --- Unix ---

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(unix)]
impl Conn for UnixStream {
    fn set_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(wait)?;
        self.set_write_timeout(wait)
    }
}

/// The user at the other end of a Unix socket.
#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(s)?.uid.as_raw())
}

#[cfg(target_os = "macos")]
fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: a connected socket, and two places to write to.
    if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(unix)]
fn same_user(s: &UnixStream) -> io::Result<bool> {
    Ok(peer_uid(s)? == rustix::process::geteuid().as_raw())
}

#[cfg(unix)]
pub struct Listener(UnixListener);

#[cfg(unix)]
impl Listener {
    fn run(self, hub: &Arc<Hub>, handler: &Arc<Handler>) -> io::Result<()> {
        loop {
            let (s, _) = match self.0.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if is_transient(&e) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(e),
            };
            if same_user(&s).unwrap_or(false) {
                spawn_conn(s, hub, handler);
            }
        }
    }
}

/// Out of descriptors or memory for a moment (ENOMEM, ENFILE, EMFILE, the same numbers on Linux and
/// macOS): not a reason to stop serving.
#[cfg(unix)]
fn is_transient(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(12 | 23 | 24)) || e.kind() == io::ErrorKind::ConnectionAborted
}

/// Linux: the daemon's lock is a listening socket already, in the abstract namespace; it serves too.
#[cfg(target_os = "linux")]
pub fn listen(lock: &UnixListener, _root_id: &str) -> io::Result<Listener> {
    Ok(Listener(lock.try_clone()?))
}

/// macOS: a socket file beside the lock, which the caller holds, so a file left there by a daemon that
/// was killed can be nobody else's.
#[cfg(target_os = "macos")]
pub fn listen(_lock: &std::fs::File, root_id: &str) -> io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    let Some(LocalAddress::Path(path)) = control_address(root_id) else {
        return Err(io::Error::other(
            "no directory for the daemon's socket: HOME is not set",
        ));
    };
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener(l))
}

#[cfg(unix)]
fn connect(address: &LocalAddress) -> io::Result<Box<dyn ClientConn>> {
    let s = match address {
        #[cfg(target_os = "linux")]
        LocalAddress::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt;
            UnixStream::connect_addr(&std::os::unix::net::SocketAddr::from_abstract_name(name)?)?
        }
        LocalAddress::Path(p) => UnixStream::connect(p)?,
        _ => return Err(io::Error::other("not an address on this platform")),
    };
    // Anyone may bind a name in the abstract namespace: an answer counts only from this user.
    if !same_user(&s)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the socket of this root's daemon belongs to another user",
        ));
    }
    Ok(Box::new(s))
}

// --- Windows ---

#[cfg(windows)]
pub use pipe::listen;
#[cfg(windows)]
pub use pipe::Listener;

#[cfg(windows)]
fn connect(address: &LocalAddress) -> io::Result<Box<dyn ClientConn>> {
    match address {
        LocalAddress::Pipe(name) => Ok(Box::new(pipe::open_trusted(name)?)),
        _ => Err(io::Error::other("not an address on this platform")),
    }
}

/// Whether to believe whoever serves a Windows pipe, decided as the tray decides (tray/src/link.rs,
/// `trust`): a pipe this user owns, which no program of another user can have made, or else one whose
/// serving process runs as this user. A daemon owns its pipe as its user for that reason, since a desktop
/// session may open the pipe of the same user's daemon in session 0 but not its process. A pipe owned by
/// Administrators, as a daemon's was before it owned its pipe, counts only by its process. An
/// administrator's program could make either, but an administrator can do anything here anyway.
#[cfg(any(windows, test))]
fn trust(
    owner: io::Result<String>,
    mine: &str,
    server: impl FnOnce() -> io::Result<(u32, String)>,
) -> io::Result<()> {
    let (owner, by_administrators) = match owner {
        Ok(o) if o == mine => return Ok(()),
        Ok(o) => (
            format!("owned by {}, not by this user ({mine})", sid_name(&o)),
            o == ADMINISTRATORS,
        ),
        Err(e) => (format!("of an owner that cannot be read ({e})"), false),
    };
    let refused = |text: String| io::Error::new(io::ErrorKind::PermissionDenied, text);
    match server() {
        Ok((_, theirs)) if theirs == mine => Ok(()),
        Ok((pid, theirs)) => Err(refused(format!(
            "the pipe of this root's daemon is {owner}, and process {pid}, which serves it, runs as {}: \
             a program of another user holds this root's address, where this user's daemon cannot \
             answer while it does",
            sid_name(&theirs)
        ))),
        Err(e) if by_administrators => Err(refused(format!(
            "the pipe of this root's daemon is {owner}, and who serves it cannot be told: {e}. A daemon \
             older than this client owns its pipe as Administrators: updating it puts this right, and \
             meanwhile an elevated session can open its process and ask"
        ))),
        Err(e) => Err(refused(format!(
            "the pipe of this root's daemon is {owner}, and who serves it cannot be told: {e}"
        ))),
    }
}

#[cfg(any(windows, test))]
const ADMINISTRATORS: &str = "S-1-5-32-544";

/// The two owners a pipe of this user's may have that are not the user, by name.
#[cfg(any(windows, test))]
fn sid_name(sid: &str) -> String {
    match sid {
        ADMINISTRATORS => format!("Administrators ({sid})"),
        "S-1-5-18" => format!("SYSTEM ({sid})"),
        _ => sid.to_string(),
    }
}

#[cfg(windows)]
mod pipe {
    //! A named pipe that only this user may open, served and read with overlapped I/O, so that every
    //! read and write can be given up after a while: a pipe has no timeouts of its own.

    use super::{spawn_conn, Conn, Handler, Hub, LocalAddress};
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Arc;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        LocalFree, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, WaitForSingleObject,
        INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    const BUFFER: u32 = 64 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last() -> io::Error {
        io::Error::last_os_error()
    }

    pub struct Pipe {
        handle: OwnedHandle,
        wait: Option<Duration>,
    }

    impl Pipe {
        fn raw(&self) -> HANDLE {
            self.handle.as_raw_handle() as HANDLE
        }

        /// One overlapped call, waited for at most `wait`; given up, it is cancelled before the
        /// OVERLAPPED it was given goes away.
        fn overlapped(
            &self,
            wait: Option<Duration>,
            call: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
        ) -> io::Result<u32> {
            // SAFETY: plain calls on a handle this owns, an event made here and closed by the
            // OwnedHandle, and an OVERLAPPED that outlives the I/O, which is waited for or cancelled
            // and then waited for.
            unsafe {
                let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
                if event.is_null() {
                    return Err(last());
                }
                let _event = OwnedHandle::from_raw_handle(event as _);
                let mut ov: OVERLAPPED = std::mem::zeroed();
                ov.hEvent = event;
                if call(self.raw(), &mut ov) == 0 {
                    let e = last();
                    match e.raw_os_error().map(|c| c as u32) {
                        Some(ERROR_IO_PENDING) => {}
                        Some(ERROR_PIPE_CONNECTED) => return Ok(0),
                        _ => return Err(e),
                    }
                    let ms = wait.map_or(INFINITE, |d| {
                        d.as_millis().min(u128::from(INFINITE - 1)) as u32
                    });
                    if WaitForSingleObject(event, ms) == WAIT_TIMEOUT {
                        CancelIoEx(self.raw(), &ov);
                        let mut n = 0;
                        GetOverlappedResult(self.raw(), &ov, &mut n, 1);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the other end did not keep up",
                        ));
                    }
                }
                let mut n = 0;
                if GetOverlappedResult(self.raw(), &ov, &mut n, 1) == 0 {
                    return Err(last());
                }
                Ok(n)
            }
        }
    }

    fn closed(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error().map(|c| c as u32),
            Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
        )
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            let ptr = buf.as_mut_ptr();
            // SAFETY: the buffer outlives the call, which `overlapped` waits for or cancels.
            match self.overlapped(self.wait, |h, ov| unsafe {
                ReadFile(h, ptr, len, std::ptr::null_mut(), ov)
            }) {
                Ok(n) => Ok(n as usize),
                Err(e) if closed(&e) => Ok(0),
                Err(e) => Err(e),
            }
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            let ptr = buf.as_ptr();
            // SAFETY: as for reading.
            match self.overlapped(self.wait, |h, ov| unsafe {
                WriteFile(h, ptr, len, std::ptr::null_mut(), ov)
            }) {
                Ok(n) => Ok(n as usize),
                Err(e) if closed(&e) => Err(io::Error::new(io::ErrorKind::BrokenPipe, e)),
                Err(e) => Err(e),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Conn for Pipe {
        fn set_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
            self.wait = wait;
            Ok(())
        }
    }

    /// A DACL that lets in this process's user and nobody else: the daemon's user, whichever session
    /// it runs in, and the same user's tray in another. The label is low, so that the tray, at medium
    /// integrity, may open what a daemon started with its highest privileges makes: the DACL decides.
    struct Security {
        descriptor: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: the descriptor is only read, by CreateNamedPipeW, and freed once.
    unsafe impl Send for Security {}

    impl Drop for Security {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.descriptor as _) };
        }
    }

    impl Security {
        /// Owned by this user, who alone may open it, at a low label so that the same user's desktop
        /// session reaches a daemon in session 0. Owned by the user, not by the token's default owner,
        /// which is Administrators for an S4U task's: a client in the desktop session may not open the
        /// daemon's process, so the pipe's owner is how it tells this user's daemon from another's.
        fn this_user_only() -> io::Result<Self> {
            let sid = user_sid()?;
            Self::from_sddl(&format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NW;;;LW)"))
        }

        fn from_sddl(sddl: &str) -> io::Result<Self> {
            let sddl = wide(sddl);
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a terminated string, and a place for the descriptor.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(last());
            }
            Ok(Self { descriptor })
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            }
        }
    }

    /// This process's user, as `S-1-5-...`.
    fn user_sid() -> io::Result<String> {
        // SAFETY: a pseudo-handle, not to be closed.
        token_user(unsafe { GetCurrentProcess() })
    }

    /// A SID as `S-1-5-...`.
    ///
    /// # Safety
    /// `sid` points to a valid SID.
    unsafe fn sid_text(sid: PSID) -> io::Result<String> {
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: a valid SID, as the caller says; the string is terminated, and freed with LocalFree.
        unsafe {
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return Err(last());
            }
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text as _);
            Ok(s)
        }
    }

    /// The owner of the pipe at the other end, read from this end, which needs nothing of the process
    /// that made it.
    fn owner(pipe: &Pipe) -> io::Result<String> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a handle this holds, opened for reading, which includes READ_CONTROL; the descriptor
        // is freed below, after the owner, which points into it, is read.
        unsafe {
            let r = GetSecurityInfo(
                pipe.raw(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            );
            if r != 0 {
                return Err(io::Error::from_raw_os_error(r as i32));
            }
            let s = sid_text(owner);
            LocalFree(sd as _);
            s
        }
    }

    /// The process that serves the pipe at the other end, and the user it runs as.
    fn server_user(pipe: &Pipe) -> io::Result<(u32, String)> {
        let mut pid = 0u32;
        // SAFETY: a pipe handle this holds, and an out-parameter.
        if unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut pid) } == 0 {
            let e = last();
            return Err(io::Error::new(
                e.kind(),
                format!("the process that serves it cannot be found ({e})"),
            ));
        }
        // SAFETY: a plain call; the handle is owned from here on.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            let e = last();
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "process {pid}, which serves it, cannot be opened to tell who it runs as ({e})"
                ),
            ));
        }
        // SAFETY: just opened.
        let process = unsafe { OwnedHandle::from_raw_handle(process as _) };
        let user = token_user(process.as_raw_handle() as HANDLE).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("process {pid}, which serves it, does not say who it runs as ({e})"),
            )
        })?;
        Ok((pid, user))
    }

    /// The user a process runs as, as `S-1-5-...`.
    fn token_user(process: HANDLE) -> io::Result<String> {
        // SAFETY: the token is closed by its OwnedHandle; the buffer is sized by the first call and
        // holds a TOKEN_USER whose SID points into it.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return Err(last());
            }
            let token = OwnedHandle::from_raw_handle(token as _);
            let mut len = 0u32;
            GetTokenInformation(
                token.as_raw_handle() as HANDLE,
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut len,
            );
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetTokenInformation(
                token.as_raw_handle() as HANDLE,
                TokenUser,
                buf.as_mut_ptr().cast(),
                len,
                &mut len,
            ) == 0
            {
                return Err(last());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            sid_text(user.User.Sid)
        }
    }

    pub struct Listener {
        name: String,
        first: Option<Pipe>,
        security: Security,
    }

    /// The first instance is made here, at once, so that a name someone else holds is an error now,
    /// not a pipe that some other program answers.
    pub fn listen(_lock: &std::fs::File, root_id: &str) -> io::Result<Listener> {
        let Some(LocalAddress::Pipe(name)) = cairn_protocol::local::control_address(root_id) else {
            return Err(io::Error::other("no pipe name"));
        };
        let security = Security::this_user_only()?;
        let first = instance(&name, &security, true)?;
        Ok(Listener {
            name,
            first: Some(first),
            security,
        })
    }

    fn instance(name: &str, security: &Security, first: bool) -> io::Result<Pipe> {
        let attributes = security.attributes();
        let mut open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: a terminated name and attributes that live through the call.
        let h = unsafe {
            CreateNamedPipeW(
                wide(name).as_ptr(),
                open,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                &attributes,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last());
        }
        Ok(Pipe {
            // SAFETY: just made, and owned from here on.
            handle: unsafe { OwnedHandle::from_raw_handle(h as _) },
            wait: None,
        })
    }

    impl Listener {
        pub(super) fn run(mut self, hub: &Arc<Hub>, handler: &Arc<Handler>) -> io::Result<()> {
            loop {
                let pipe = match self.first.take() {
                    Some(p) => p,
                    None => instance(&self.name, &self.security, false)?,
                };
                // SAFETY: the OVERLAPPED is waited for by `overlapped`.
                match pipe.overlapped(None, |h, ov| unsafe { ConnectNamedPipe(h, ov) }) {
                    Ok(_) => spawn_conn(pipe, hub, handler),
                    // The client came and went before it was seen.
                    Err(e) if closed(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }

    /// The client's end, once whoever serves it is believed to be this user's daemon, by
    /// [`super::trust`]: before a word is sent or read.
    pub fn open_trusted(name: &str) -> io::Result<Pipe> {
        let pipe = open(name)?;
        super::trust(owner(&pipe), &user_sid()?, || server_user(&pipe))?;
        Ok(pipe)
    }

    /// The client's end, unchecked. Identification only: the daemon learns who asks, and cannot act as
    /// them.
    fn open(name: &str) -> io::Result<Pipe> {
        let mut tries = 0;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name)
            {
                Ok(f) => {
                    return Ok(Pipe {
                        handle: OwnedHandle::from(f),
                        wait: None,
                    })
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && tries < 4 => {
                    tries += 1;
                    // SAFETY: a terminated name.
                    unsafe { WaitNamedPipeW(wide(name).as_ptr(), 500) };
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The owner as a client reads it from its own end, as the tray does: this user, whether or not
        /// this process runs elevated, whose token's default owner is then Administrators.
        #[test]
        fn the_pipe_is_owned_by_its_user() {
            let root_id = format!("test-{:032x}", rand::random::<u128>());
            let lock = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
            let listener = listen(&lock, &root_id).unwrap();
            let client = open(&listener.name).unwrap();
            assert_eq!(owner(&client).unwrap(), user_sid().unwrap());
        }

        /// What `Client::connect` opens: this user's daemon, by its pipe's owner; a daemon's pipe as one
        /// older than this made it, by its process, which is this one; and not a pipe another user
        /// serves. lsass's, on every Windows, owned by Administrators and served as SYSTEM, stands in for
        /// a program of another user that took a root's address.
        #[test]
        fn a_client_believes_its_users_daemon_and_refuses_another_users_pipe() {
            let connect = |name: &str| super::super::connect(&LocalAddress::Pipe(name.into()));
            let root_id = format!("test-{:032x}", rand::random::<u128>());
            let lock = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
            let listener = listen(&lock, &root_id).unwrap();
            connect(&listener.name).unwrap();

            // Owned by the token's default owner: Administrators when this runs elevated, as from SSH.
            let sid = user_sid().unwrap();
            let old = Security::from_sddl(&format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;LW)")).unwrap();
            let old_name = format!("{}-old", listener.name);
            let _old = instance(&old_name, &old, true).unwrap();
            connect(&old_name).unwrap();

            let lsass = r"\\.\pipe\lsass";
            let raw = open(lsass).expect("lsass's pipe opens, for reading and writing");
            assert_ne!(owner(&raw).unwrap(), sid);
            drop(raw);
            let e = connect(lsass)
                .err()
                .expect("a pipe another user serves is refused");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                e.to_string()
                    .starts_with("the pipe of this root's daemon is owned by"),
                "{e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THIS_USER: &str = "S-1-5-21-1-2-3-1001";
    const OTHER: &str = "S-1-5-21-1-2-3-1002";

    /// Windows' decision, on every platform: the pipe's owner first, then the serving process's user.
    #[test]
    fn a_windows_pipe_is_believed_by_its_owner_or_else_by_its_serving_process() {
        let never = || -> io::Result<(u32, String)> {
            panic!("a pipe this user owns needs nothing of its process")
        };
        trust(Ok(THIS_USER.into()), THIS_USER, never).unwrap();

        // A daemon older than its pipe's owner: Administrators own it, so its process decides.
        trust(Ok(ADMINISTRATORS.into()), THIS_USER, || {
            Ok((7, THIS_USER.into()))
        })
        .unwrap();
        let e = trust(Ok(ADMINISTRATORS.into()), THIS_USER, || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "process 7, which serves it, cannot be opened to tell who it runs as",
            ))
        })
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let text = e.to_string();
        assert!(
            text.contains("owned by Administrators (S-1-5-32-544), not by this user")
                && text.contains("process 7, which serves it, cannot be opened")
                && text.contains("updating it puts this right"),
            "{text}"
        );

        let e = trust(Ok(OTHER.into()), THIS_USER, || Ok((9, OTHER.into()))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            e.to_string()
                .contains("process 9, which serves it, runs as"),
            "{e}"
        );
        trust(Ok(ADMINISTRATORS.into()), THIS_USER, || {
            Ok((9, "S-1-5-18".into()))
        })
        .unwrap_err();
        // Only a pipe Administrators own is taken for a daemon from before, which updating puts right.
        let e = trust(Ok(OTHER.into()), THIS_USER, || {
            Err(io::Error::other("no process"))
        })
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(!e.to_string().contains("older"), "{e}");

        // An owner that cannot be read counts for nothing either way.
        trust(Err(io::Error::other("unreadable")), THIS_USER, || {
            Ok((7, THIS_USER.into()))
        })
        .unwrap();
        trust(Err(io::Error::other("unreadable")), THIS_USER, || {
            Ok((9, OTHER.into()))
        })
        .unwrap_err();
    }
}
