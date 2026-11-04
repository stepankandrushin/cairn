//! One connection to a daemon's local socket, `cairn_protocol::local`: a request line out, reply lines in.
//! Before a word is read, the other end is checked to run as this user, so a program of another user that
//! took the address first is refused rather than believed.

use cairn_protocol::local::{control_address, LocalAddress, Reply, Request};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

/// Longest line taken from a daemon: a status is far smaller.
const MAX_LINE: u64 = 16 << 20;

#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Stream = std::fs::File;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// Nothing answers at the daemon's address.
    NotRunning(String),
    /// Something answers, but not as a daemon of this user should.
    Refused(String),
    Other(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::NotRunning(s) | LinkError::Refused(s) | LinkError::Other(s) => {
                f.write_str(s)
            }
        }
    }
}

pub fn connect(root_id: &str) -> Result<Stream, LinkError> {
    let addr = control_address(root_id)
        .ok_or_else(|| LinkError::Other("this platform has no daemon socket".into()))?;
    let stream = open(&addr)?;
    check_peer(&stream)?;
    Ok(stream)
}

fn not_running(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), NotFound | ConnectionRefused | AddrNotAvailable)
}

#[cfg(unix)]
fn open(addr: &LocalAddress) -> Result<Stream, LinkError> {
    let r = match addr {
        #[cfg(target_os = "linux")]
        LocalAddress::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt;
            std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
                .and_then(|a| Stream::connect_addr(&a))
        }
        LocalAddress::Path(p) => Stream::connect(p),
        other => return Err(LinkError::Other(format!("no way to reach {other:?} here"))),
    };
    r.map_err(|e| {
        if not_running(&e) {
            LinkError::NotRunning("no daemon answers".into())
        } else {
            LinkError::Other(format!("cannot reach the daemon: {e}"))
        }
    })
}

#[cfg(windows)]
fn open(addr: &LocalAddress) -> Result<Stream, LinkError> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_PIPE_BUSY: i32 = 231;
    // SECURITY_SQOS_PRESENT | SecurityIdentification: the daemon may learn who asks, not act as them.
    const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
    let LocalAddress::Pipe(name) = addr else {
        return Err(LinkError::Other(format!("no way to reach {addr:?} here")));
    };
    let mut tries = 0;
    loop {
        let r = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(name);
        match r {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) if not_running(&e) => {
                return Err(LinkError::NotRunning("no daemon answers".into()))
            }
            Err(e) => return Err(LinkError::Other(format!("cannot reach the daemon: {e}"))),
        }
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &Stream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: a valid socket descriptor and two out-parameters.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &Stream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid socket descriptor, and a buffer of the size given.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

#[cfg(unix)]
fn check_peer(stream: &Stream) -> Result<(), LinkError> {
    let theirs = peer_uid(stream)
        .map_err(|e| LinkError::Refused(format!("cannot tell who answers: {e}")))?;
    // SAFETY: no arguments, cannot fail.
    let mine = unsafe { libc::geteuid() };
    if theirs != mine {
        return Err(LinkError::Refused(format!(
            "a program of user {theirs}, not this one, answers at the daemon's address"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn check_peer(pipe: &Stream) -> Result<(), LinkError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut pid = 0u32;
    // SAFETY: a pipe handle this process holds, and an out-parameter.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(LinkError::Refused(format!(
            "cannot tell who answers: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: plain calls; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(LinkError::Refused(format!(
            "cannot tell who answers, process {pid}: {}",
            std::io::Error::last_os_error()
        )));
    }
    let theirs = token_user(process);
    // SAFETY: the handle opened above.
    unsafe { CloseHandle(process) };
    let theirs = theirs.map_err(|e| LinkError::Refused(format!("cannot tell who answers: {e}")))?;
    // SAFETY: a pseudo-handle, not to be closed.
    let mine = token_user(unsafe { GetCurrentProcess() })
        .map_err(|e| LinkError::Other(format!("cannot tell who this is: {e}")))?;
    if theirs != mine {
        return Err(LinkError::Refused(format!(
            "a program of another user (process {pid}) answers at the daemon's address"
        )));
    }
    Ok(())
}

/// The SID a process runs as, as bytes: two are the same user when the bytes are the same.
#[cfg(windows)]
fn token_user(process: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    let mut token = std::ptr::null_mut();
    // SAFETY: a process handle and an out-parameter; the token is closed below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut len = 0u32;
    // SAFETY: asks only for the size.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    // u64s, so that the TOKEN_USER at its start is aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: a buffer of at least `len` bytes.
    let ok =
        unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) };
    let err = std::io::Error::last_os_error();
    // SAFETY: the token opened above.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(err);
    }
    // SAFETY: GetTokenInformation wrote a TOKEN_USER whose SID points into `buf`.
    unsafe {
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let sid = user.User.Sid;
        let n = GetLengthSid(sid) as usize;
        Ok(std::slice::from_raw_parts(sid as *const u8, n).to_vec())
    }
}

pub fn send(stream: &mut Stream, request: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()
}

/// The next reply, or `None` once the daemon has closed.
pub fn read_reply<R: Read>(reader: &mut BufReader<R>) -> Result<Option<Reply>, String> {
    let mut line = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("the daemon stopped answering: {e}"))?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') && n as u64 == MAX_LINE {
        return Err("the daemon sent a line too long to read".into());
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| format!("the daemon said something this tray cannot read: {e}"))
}

/// One request and its first reply, given up on after `timeout`.
pub fn ask(root_id: &str, request: Request, timeout: Duration) -> Result<Reply, LinkError> {
    let root_id = root_id.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = (|| {
            let mut stream = connect(&root_id)?;
            send(&mut stream, &request)
                .map_err(|e| LinkError::Other(format!("cannot ask the daemon: {e}")))?;
            let mut reader = BufReader::new(stream);
            match read_reply(&mut reader).map_err(LinkError::Other)? {
                Some(reply) => Ok(reply),
                None => Err(LinkError::Other(
                    "the daemon closed without an answer".into(),
                )),
            }
        })();
        let _ = tx.send(r);
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        Err(LinkError::Other(format!(
            "the daemon did not answer within {} s",
            timeout.as_secs()
        )))
    })
}

/// [`ask`] for a request whose answer is [`Reply::Done`].
pub fn tell(root_id: &str, request: Request) -> Result<(), String> {
    match ask(root_id, request, Duration::from_secs(10)) {
        Ok(Reply::Done) => Ok(()),
        Ok(Reply::Error { text }) => Err(text),
        Ok(other) => Err(format!("the daemon answered {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}
