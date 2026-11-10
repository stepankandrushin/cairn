//! The HTTP client. Every call is safe to retry: a commit by its operation ID (the server answers a
//! retry of one it took with the first answer), chunks by their hash, and the rest are reads. So a
//! request the network failed in a way that says nothing about the request is tried again, over about
//! a minute in a sync and a few seconds in a command a person waits on, each time said; past that the
//! sync fails, and a later one picks up from durable state.

use crate::root::Config;
use anyhow::{anyhow, bail, Context, Result};
use cairn_protocol::wire::{
    self, ChangesResponse, CommitRequest, CommitResponse, ConflictEntry, HashList, Info,
};
use cairn_protocol::wire::{Access, Grant, Me, Member};
use cairn_protocol::{Hash, Op};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use ureq::tls::{RootCerts, TlsConfig};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Agent, Timeout};

/// How long one lookup of the server's name serves. ureq looks the name up before every request,
/// even one that goes over a connection it already holds, and a slow resolver then costs more than
/// the request: one sync of one small file made 8 lookups, at 55 to 125 ms each. A minute bounds
/// how long a server that moved is looked for at its old address.
const NAME_TTL: Duration = Duration::from_secs(60);

/// The server's name could not be looked up: a network not up yet, as on a laptop just woken, or a
/// name that does not exist (yet). Tried again like any failure of the network.
#[derive(Debug, thiserror::Error)]
#[error("cannot look up the server's name ({0})")]
struct LookupFailed(ureq::Error);

/// The resolver, with the last answer kept for [`NAME_TTL`]. A failed lookup is not kept.
struct CachedResolver<R> {
    inner: R,
    last: Mutex<Option<(String, Instant, ResolvedSocketAddrs)>>,
}

impl<R> std::fmt::Debug for CachedResolver<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CachedResolver")
    }
}

impl<R: Resolver> Resolver for CachedResolver<R> {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let key = format!(
            "{}://{}",
            uri.scheme_str().unwrap_or(""),
            uri.authority().map_or("", |a| a.as_str())
        );
        if let Some((k, at, addrs)) = &*self.last.lock().unwrap_or_else(|e| e.into_inner()) {
            if *k == key && at.elapsed() < NAME_TTL {
                return Ok(addrs.clone());
            }
        }
        let addrs = self
            .inner
            .resolve(uri, config, timeout)
            .map_err(|e| ureq::Error::Other(Box::new(LookupFailed(e))))?;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((key, Instant::now(), addrs.clone()));
        Ok(addrs)
    }
}

/// The server said its disk is full; nothing there changed.
#[derive(Debug, thiserror::Error)]
#[error("the server's disk is full: nothing was lost; free space on the server and sync again")]
pub struct ServerFull;

/// The server refused a request this token's access does not cover (403), as an upload or a commit
/// in a folder its user may only read. Nothing there changed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Forbidden(pub String);

/// Whether an error chain is a [`Forbidden`] answer.
pub fn is_forbidden(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Forbidden>())
}

/// How hard a client tries a request again that the network failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retries {
    /// A sync, the command's or the daemon's: 0.5, 1, 2, 4, 8, 16 and 30 s, about a minute.
    Sync,
    /// A command a person waits on: 0.5, 1 and 2 s, so that a mistyped address fails in about 3.5 s.
    Short,
    /// Each request once: the daemon's feed, a loop that tries again on its own and whose failing says
    /// the server is down.
    Never,
}

impl Retries {
    /// The waits before each new try, in half seconds. A harness build shortens the half second
    /// (`client.retry_ms`).
    fn waits(self) -> &'static [u32] {
        match self {
            Retries::Sync => &[1, 2, 4, 8, 16, 32, 60],
            Retries::Short => &[1, 2, 4],
            Retries::Never => &[],
        }
    }

    fn span(self) -> &'static str {
        match self {
            Retries::Sync => "about a minute",
            Retries::Short | Retries::Never => "a few seconds",
        }
    }
}

/// The most one request takes, its tries and the waits between them together, give or take a try
/// under way: a server whose host drops every packet costs 30 s a connect.
const MAX_REQUEST_TIME: Duration = Duration::from_secs(180);

/// The wait before the next try, if there is one: the budget's next, unless it would take the request
/// past `cap`.
fn next_wait(
    waits: &[u32],
    tries: usize,
    elapsed: Duration,
    unit: Duration,
    cap: Duration,
) -> Option<Duration> {
    let wait = unit * *waits.get(tries)?;
    (elapsed + wait <= cap).then_some(wait)
}

/// ENOBUFS: the kernel had no memory for the socket's buffers. std gives it no kind of its own.
#[cfg(target_os = "macos")]
const ENOBUFS: i32 = 55;
/// WSAENOBUFS.
#[cfg(windows)]
const ENOBUFS: i32 = 10055;
#[cfg(not(any(target_os = "macos", windows)))]
const ENOBUFS: i32 = 105;

/// Where new tries are said. Unset, each try is, on standard error, for a person watching.
static RETRY_LOG: OnceLock<fn(&str)> = OnceLock::new();

/// Say new tries in `log`, as the daemon does in its own: a request's first and its outcome alone, as
/// a log is read later, and a line a try would fill it while a server is down. The first call decides.
pub fn log_retries_with(log: fn(&str)) {
    let _ = RETRY_LOG.set(log);
}

/// A new try: each one on standard error; in a log set by [`log_retries_with`], only the first.
fn say_try(error: &anyhow::Error, wait: Duration, tries: usize, retries: Retries) {
    match RETRY_LOG.get() {
        Some(log) if tries == 1 => log(&format!(
            "{error:#}; trying again for up to {}",
            retries.span()
        )),
        Some(_) => {}
        None => {
            let _ = writeln!(
                io::stderr(),
                "{error:#}; trying again in {}, {tries} of {}",
                seconds(wait),
                retries.waits().len()
            );
        }
    }
}

/// How a request that was tried again ended.
fn say_outcome(line: &str) {
    match RETRY_LOG.get() {
        Some(log) => log(line),
        None => {
            let _ = writeln!(io::stderr(), "{line}");
        }
    }
}

/// A failure of the network that says nothing about the request: the kernel out of memory for a
/// socket, a connection reset, aborted, refused, timed out or closed before the whole answer came, as a
/// server restarting closes it, a network or host out of reach. A refusal, an answer that makes no
/// sense, or anything else is the request's own, and a new try would not help.
fn transient_io(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    e.raw_os_error() == Some(ENOBUFS)
        || matches!(
            e.kind(),
            OutOfMemory
                | ConnectionReset
                | ConnectionAborted
                | BrokenPipe
                | TimedOut
                | ConnectionRefused
                | NetworkDown
                | NetworkUnreachable
                | HostUnreachable
                | UnexpectedEof
        )
}

/// As [`transient_io`], in ureq's terms, and a name that could not be looked up. ureq reports any
/// ETIMEDOUT as a timeout of the step it was in, which is the kernel's unless it is one this client
/// sets: the connect's 30 s is ETIMEDOUT's other face and is tried again, the 600 s wait for an answer
/// is a server that is there and not answering, and is not.
fn transient(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Io(e) => transient_io(e),
        ureq::Error::Other(e) => e.is::<LookupFailed>(),
        ureq::Error::HostNotFound => true,
        ureq::Error::Timeout(t) => matches!(
            t,
            Timeout::Connect
                | Timeout::SendRequest
                | Timeout::Await100
                | Timeout::SendBody
                | Timeout::RecvBody
                | Timeout::PerRead
                | Timeout::PerWrite
        ),
        _ => false,
    }
}

/// What a proxy says when the server behind it is down, restarting or slow: 502, 503, 504. Never a
/// 4xx, which is about the request, or 507, the server's disk full.
fn transient_status(status: u16) -> bool {
    matches!(status, 502..=504)
}

/// "0.5 s", "4 s".
fn seconds(d: Duration) -> String {
    let ms = d.as_millis();
    if ms % 1000 == 0 {
        format!("{} s", ms / 1000)
    } else {
        format!("{} s", ms as f64 / 1000.0)
    }
}

/// One try of a request that did not get its answer.
struct Failure {
    error: anyhow::Error,
    /// The network's failure, which says nothing about the request: worth another try.
    transient: bool,
}

impl Failure {
    fn of(what: &str, e: ureq::Error) -> Self {
        Self {
            transient: transient(&e),
            error: anyhow::Error::new(e).context(format!("{what}: cannot reach the server")),
        }
    }
}

pub struct Client {
    agent: Agent,
    /// The server's URL, for what is not about one namespace.
    server: String,
    base: String,
    auth: String,
    /// What this token may do in the namespace, as the server said when this client was made. `None`
    /// from a server before accounts, whose one token may do everything.
    access: Option<Access>,
    /// That the server takes a note naming more objects than one: a commit's notes of one text go
    /// as one.
    note_groups: bool,
    /// How hard a request the network failed is tried again.
    retries: Retries,
    /// How many times a request was tried again, for the sync's report.
    retried: Arc<AtomicU64>,
}

const MAX_RESPONSE: u64 = 1 << 40;

fn host_is_loopback(url: &str) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl Client {
    pub fn new(config: &Config, retries: Retries) -> Result<Self> {
        let mut client = Self::unchecked(config, retries)?;
        let info = client.info()?;
        if info.protocol != cairn_protocol::PROTOCOL_VERSION {
            bail!(
                "the server speaks protocol {}, this client {}: upgrade whichever is older",
                info.protocol,
                cairn_protocol::PROTOCOL_VERSION
            );
        }
        client.access = info.access;
        client.note_groups = info.note_groups;
        Ok(client)
    }

    /// A client of the server alone, for what is not about one namespace: who the token is, and what
    /// it may reach.
    pub fn of_server(server: &str, token: &str, allow_http: bool) -> Result<Self> {
        Self::unchecked(
            &Config {
                server: server.trim_end_matches('/').to_string(),
                namespace: String::new(),
                token: token.trim().to_string(),
                device: cairn_protocol::DeviceId::random(),
                device_name: String::new(),
                policy: Default::default(),
                filenames: Default::default(),
                allow_http,
            },
            Retries::Short,
        )
    }

    /// What this token may do here, as the server said when this client was made.
    pub fn access(&self) -> Option<Access> {
        self.access
    }

    /// Whether this token may only read here: nothing changed here may go up.
    pub fn read_only(&self) -> bool {
        self.access.is_some_and(|a| !a.can_write())
    }

    /// The protocol the server speaks, whichever it is: for the updater, which takes a release on
    /// another protocol only once the server speaks that one.
    pub fn server_protocol(config: &Config) -> Result<u32> {
        Ok(Self::unchecked(config, Retries::Short)?.info()?.protocol)
    }

    fn unchecked(config: &Config, retries: Retries) -> Result<Self> {
        let url = &config.server;
        if url.starts_with("http://") {
            if !host_is_loopback(url) && !config.allow_http {
                bail!("{url} is plain HTTP beyond loopback, which sends the token in the clear: use https://, or set allow_http");
            }
        } else if !url.starts_with("https://") {
            bail!("{url} is not an http:// or https:// URL");
        }
        let config_http = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        let agent = Agent::with_parts(
            config_http,
            DefaultConnector::default(),
            CachedResolver {
                inner: DefaultResolver::default(),
                last: Mutex::new(None),
            },
        );
        let client = Self {
            agent,
            server: url.to_string(),
            base: format!("{url}/v1/ns/{}", config.namespace),
            auth: format!("Bearer {}", config.token),
            access: None,
            note_groups: false,
            retries,
            retried: Arc::new(AtomicU64::new(0)),
        };
        Ok(client)
    }

    /// Counts each new try of a request, from now on.
    pub fn retried(&self) -> Arc<AtomicU64> {
        self.retried.clone()
    }

    /// Send a request and read its whole answer, trying again while the network fails it in a way that
    /// says nothing about the request. `point` names it for the harness's failpoints.
    fn exchange(
        &self,
        what: &str,
        point: &str,
        send: impl Fn() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Vec<u8>> {
        let unit = Duration::from_millis(cairn_failpoint::tuned("client.retry_ms", 500));
        let cap = Duration::from_millis(cairn_failpoint::tuned(
            "client.request_ms",
            MAX_REQUEST_TIME.as_millis() as u64,
        ));
        let started = Instant::now();
        let mut tries = 0;
        loop {
            let failure = match self.once(what, point, &send) {
                Ok(body) => {
                    if tries > 0 {
                        say_outcome(&format!(
                            "{what}: answered on try {} after {:.1} s",
                            tries + 1,
                            started.elapsed().as_secs_f64()
                        ));
                    }
                    return Ok(body);
                }
                Err(f) => f,
            };
            let wait = match failure.transient {
                true => next_wait(self.retries.waits(), tries, started.elapsed(), unit, cap),
                false => None,
            };
            let Some(wait) = wait else {
                if tries == 0 {
                    return Err(failure.error);
                }
                say_outcome(&format!(
                    "{what}: gave up on try {} after {:.1} s",
                    tries + 1,
                    started.elapsed().as_secs_f64()
                ));
                if !failure.transient {
                    return Err(failure.error);
                }
                // As a sync fails without new tries, saying how hard it tried.
                return Err(anyhow!("{:#}; tried {} times", failure.error, tries + 1));
            };
            tries += 1;
            self.retried.fetch_add(1, Ordering::SeqCst);
            say_try(&failure.error, wait, tries, self.retries);
            std::thread::sleep(wait);
        }
    }

    fn once(
        &self,
        what: &str,
        point: &str,
        send: &impl Fn() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Vec<u8>, Failure> {
        cairn_failpoint::network(point, false).map_err(|e| Failure::of(what, e.into()))?;
        let mut resp = send().map_err(|e| Failure::of(what, e))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_vec()
            .map_err(|e| Failure {
                transient: transient(&e),
                error: anyhow::Error::new(e).context(format!("{what}: reading the response")),
            })?;
        // As if the connection were lost with the answer, which the server has sent all the same.
        cairn_failpoint::network(point, true).map_err(|e| Failure {
            transient: transient_io(&e),
            error: anyhow::Error::new(ureq::Error::Io(e))
                .context(format!("{what}: reading the response")),
        })?;
        let refused = |error: anyhow::Error| Failure {
            error,
            transient: transient_status(status),
        };
        match status {
            200..=299 => Ok(body),
            401 => Err(refused(anyhow!(
                "{what}: the server refused the access token"
            ))),
            507 => Err(refused(anyhow!(ServerFull))),
            403 => Err(refused(anyhow::Error::new(Forbidden(format!(
                "{what}: {}",
                String::from_utf8_lossy(&body)
            ))))),
            other => Err(refused(anyhow!(
                "{what}: server answered {other}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
    }

    fn get_json<T: DeserializeOwned>(&self, what: &str, point: &str, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base);
        let body = self.exchange(what, point, || {
            self.agent
                .get(&url)
                .header("Authorization", &self.auth)
                .call()
        })?;
        Ok(serde_json::from_slice(&body)?)
    }

    fn post_bytes(
        &self,
        what: &str,
        point: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let url = format!("{}{path}", self.base);
        self.exchange(what, point, || {
            self.agent
                .post(&url)
                .header("Authorization", &self.auth)
                .header("Content-Type", content_type)
                .send(body)
        })
    }

    fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        what: &str,
        point: &str,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let bytes = self.post_bytes(
            what,
            point,
            path,
            "application/json",
            &serde_json::to_vec(body)?,
        )?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn info(&self) -> Result<Info> {
        self.get_json("server info", "client.info", "/info")
    }

    pub fn missing(&self, hashes: &[Hash]) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        for batch in hashes.chunks(4096) {
            let resp: HashList = self.post_json(
                "missing chunks",
                "client.missing",
                "/chunks/missing",
                &HashList {
                    hashes: batch.to_vec(),
                },
            )?;
            out.extend(resp.hashes);
        }
        Ok(out)
    }

    pub fn upload(&self, frames: &[u8]) -> Result<()> {
        self.post_bytes(
            "uploading chunks",
            "client.upload",
            "/chunks/upload",
            "application/octet-stream",
            frames,
        )?;
        Ok(())
    }

    /// Fetch and verify a batch of chunks; a damaged or short answer is an error.
    pub fn download(&self, hashes: &[Hash]) -> Result<Vec<(Hash, Vec<u8>)>> {
        let body = self.post_bytes(
            "downloading chunks",
            "client.download",
            "/chunks/download",
            "application/json",
            &serde_json::to_vec(&HashList {
                hashes: hashes.to_vec(),
            })?,
        )?;
        let frames = wire::read_frames(&body).context("downloading chunks")?;
        check_answer(hashes, &frames)?;
        crate::progress::downloaded(frames.iter().map(|(_, d)| d.len() as u64).sum());
        Ok(frames.into_iter().map(|(h, d)| (h, d.to_vec())).collect())
    }

    /// A commit as queued, a note to each object; to a server that takes them, its notes of one text
    /// go as one. The same ID either way, so a commit sent once each way lands once.
    pub fn commit(&self, req: &CommitRequest) -> Result<CommitResponse> {
        if self.note_groups && req.ops.iter().any(|op| matches!(op, Op::Note { .. })) {
            let grouped = CommitRequest {
                ops: cairn_protocol::group_notes(&req.ops),
                ..req.clone()
            };
            return self.post_json("committing", "client.commit", "/commit", &grouped);
        }
        self.post_json("committing", "client.commit", "/commit", req)
    }

    pub fn changes(&self, after: u64) -> Result<ChangesResponse> {
        self.get_json(
            "reading changes",
            "client.changes",
            &format!("/changes?after={after}&limit=1000"),
        )
    }

    /// The journal entry at `seq`, without its operations, and the head: where this device is, and
    /// whether anything came after it, for the price of one entry however large its commit. An older
    /// server sends the operations too.
    pub fn change_at(&self, seq: u64) -> Result<ChangesResponse> {
        let query = match seq {
            0 => "after=0&limit=0".to_string(),
            seq => format!("after={}&limit=1&ops=false", seq - 1),
        };
        self.get_json(
            "reading the journal",
            "client.changes",
            &format!("/changes?{query}"),
        )
    }

    /// The head, once it is not `after` or `wait` has gone by. A plain request and answer, so it
    /// passes through any reverse proxy. Bounded here too: a connection a firewall silently dropped
    /// answers nothing, ever.
    pub fn wait_for_head(&self, after: u64, wait: Duration) -> Result<u64> {
        let url = format!(
            "{}/changes?after={after}&limit=0&wait={}",
            self.base,
            wait.as_secs()
        );
        let body = self.exchange("waiting for changes", "client.wait", || {
            self.agent
                .get(&url)
                .config()
                .timeout_recv_response(Some(wait + Duration::from_secs(15)))
                .build()
                .header("Authorization", &self.auth)
                .call()
        })?;
        let answer: ChangesResponse = serde_json::from_slice(&body)?;
        Ok(answer.head)
    }

    pub fn conflicts(&self) -> Result<Vec<ConflictEntry>> {
        self.get_json("reading conflicts", "client.conflicts", "/conflicts")
    }

    /// Who this token is and every folder it reaches. A server before accounts has no such question
    /// to answer.
    pub fn me(&self) -> Result<Me> {
        let url = format!("{}/v1/me", self.server);
        let body = self
            .exchange("asking who this token is", "client.me", || {
                self.agent
                    .get(&url)
                    .header("Authorization", &self.auth)
                    .call()
            })
            .map_err(|e| {
                if format!("{e:#}").contains("server answered 404") {
                    anyhow!("this server has no accounts, so its one token reaches every folder: upgrade it to have users")
                } else {
                    e
                }
            })?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// An address of the server's web page that signs a browser in as this token, once and within two
    /// minutes, open at `folder`'s top: what `cairn web` and the tray's Open in browser open.
    pub fn sign_in_url(&self, folder: Option<&str>) -> Result<String> {
        let url = format!("{}/v1/session/ticket", self.server);
        let body = self
            .exchange("asking for a sign-in to the web page", "client.ticket", || {
                self.agent
                    .post(&url)
                    .header("Authorization", &self.auth)
                    .send_empty()
            })
            .map_err(|e| {
                if format!("{e:#}").contains("server answered 404") {
                    anyhow!("this server's web page cannot be signed in to from here: upgrade the server, or sign in there with a token")
                } else {
                    e
                }
            })?;
        let t: cairn_protocol::web::SignInTicket = serde_json::from_slice(&body)?;
        Ok(cairn_protocol::web::sign_in_url(
            &self.server,
            &t.ticket,
            folder,
        ))
    }

    /// Who has access to this folder, at which level.
    pub fn members(&self) -> Result<Vec<Member>> {
        self.get_json("reading who has access", "client.access", "/access")
    }

    /// Give `user` this level here, or with `None` take all of it away. Only a manager may.
    pub fn set_member(&self, user: &str, access: Option<Access>) -> Result<()> {
        let url = format!("{}/access/{user}", self.base);
        let body = match access {
            Some(access) => Some(serde_json::to_vec(&Grant { access })?),
            None => None,
        };
        self.exchange("changing who has access", "client.access", || match &body {
            Some(b) => self
                .agent
                .put(&url)
                .header("Authorization", &self.auth)
                .header("Content-Type", "application/json")
                .send(b.as_slice()),
            None => self
                .agent
                .delete(&url)
                .header("Authorization", &self.auth)
                .call(),
        })?;
        Ok(())
    }
}

/// Each frame is valid by its own hash; it must also be the chunk asked for, in the place asked.
fn check_answer(asked: &[Hash], frames: &[(Hash, &[u8])]) -> Result<()> {
    if frames.len() != asked.len() {
        bail!(
            "downloading chunks: asked for {}, got {}",
            asked.len(),
            frames.len()
        );
    }
    for (want, (got, _)) in asked.iter().zip(frames) {
        if want != got {
            bail!("downloading chunks: asked for {want}, the server sent {got}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server, or anything between it and here, answering with a well-formed chunk of its choosing.
    fn hostile_server(answer: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                let (head_end, length) = loop {
                    let n = stream.read(&mut buf).unwrap();
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (end + 4, length);
                    }
                };
                while request.len() < head_end + length {
                    let n = stream.read(&mut buf).unwrap();
                    request.extend_from_slice(&buf[..n]);
                }
                let body = if request.starts_with(b"GET") {
                    format!(
                        r#"{{"protocol":{},"server_version":"x","head":0}}"#,
                        cairn_protocol::PROTOCOL_VERSION
                    )
                    .into_bytes()
                } else {
                    answer.clone()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        format!("http://{addr}")
    }

    /// The connection closed with no answer, as by a server killed or restarting.
    const CLOSE: u16 = 0;
    /// The connection closed partway through the answer.
    const CUT: u16 = 1;

    /// A server, or a proxy before it, answering each request with the next of `script`, a status,
    /// [`CLOSE`] or [`CUT`] (the last one from then on), and counting the requests.
    fn scripted_server(script: Vec<u16>) -> (String, Arc<AtomicU64>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicU64::new(0));
        let count = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let n = count.fetch_add(1, Ordering::SeqCst) as usize;
                let status = script[n.min(script.len() - 1)];
                if status == CLOSE {
                    continue;
                }
                let body = match status {
                    200 | CUT => format!(
                        r#"{{"protocol":{},"server_version":"x","head":0}}"#,
                        cairn_protocol::PROTOCOL_VERSION
                    ),
                    _ => "a proxy's page".to_string(),
                };
                let head = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    if status == CUT { 200 } else { status },
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                let sent = if status == CUT {
                    body.len() / 2
                } else {
                    body.len()
                };
                stream.write_all(&body.as_bytes()[..sent]).unwrap();
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn config(server: String) -> Config {
        Config {
            server,
            namespace: "default".into(),
            token: "t".into(),
            device: cairn_protocol::DeviceId::random(),
            device_name: "d".into(),
            policy: Default::default(),
            filenames: Default::default(),
            allow_http: false,
        }
    }

    #[test]
    fn a_proxy_whose_server_is_away_is_asked_again() {
        let (url, seen) = scripted_server(vec![502, 503, 504, 200]);
        let client = Client::new(&config(url), Retries::Sync).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 4);
        assert_eq!(client.retried().load(Ordering::SeqCst), 3);
    }

    /// As a server restarting does: the connection closed before the answer, or partway through it.
    #[test]
    fn a_connection_closed_before_or_during_the_answer_is_tried_again() {
        let (url, seen) = scripted_server(vec![CLOSE, CUT, 200]);
        let client = Client::new(&config(url), Retries::Sync).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        assert_eq!(client.retried().load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_refusal_or_a_full_disk_is_not_asked_again() {
        for status in [400, 401, 404, 409, 413, 500, 507] {
            let (url, seen) = scripted_server(vec![status, 200]);
            assert!(
                Client::new(&config(url), Retries::Sync).is_err(),
                "{status}"
            );
            assert_eq!(seen.load(Ordering::SeqCst), 1, "{status}");
        }
    }

    /// A command a person waits on: three tries more, over 3.5 s, then the error.
    #[test]
    fn a_command_tries_again_for_a_few_seconds() {
        let (url, seen) = scripted_server(vec![503]);
        let started = Instant::now();
        let err = Client::new(&config(url), Retries::Short).err().unwrap();
        let took = started.elapsed();
        assert_eq!(seen.load(Ordering::SeqCst), 4);
        assert!(format!("{err:#}").ends_with("; tried 4 times"), "{err:#}");
        assert!(
            took >= Duration::from_millis(3500) && took < Duration::from_secs(10),
            "{took:?}"
        );
    }

    #[test]
    fn the_feed_asks_once() {
        let (url, seen) = scripted_server(vec![503, 200]);
        let err = Client::new(&config(url), Retries::Never).err().unwrap();
        assert!(err.to_string().contains("503"), "{err}");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    /// A name that does not exist is looked up again, as a network not yet up after a laptop wakes
    /// says the same of every name.
    #[test]
    fn a_name_that_cannot_be_looked_up_is_tried_again() {
        let r = CachedResolver {
            inner: DefaultResolver::default(),
            last: Mutex::new(None),
        };
        let uri: ureq::http::Uri = "http://cairn-test.invalid:1/v1/info".parse().unwrap();
        let err = r
            .resolve(
                &uri,
                &ureq::config::Config::default(),
                NextTimeout::default(),
            )
            .unwrap_err();
        assert!(transient(&err), "{err}");
        assert!(
            err.to_string().contains("cannot look up the server's name"),
            "{err}"
        );
    }

    #[test]
    fn only_failures_of_the_network_are_tried_again() {
        use io::ErrorKind::*;
        for kind in [
            ConnectionReset,
            ConnectionAborted,
            BrokenPipe,
            TimedOut,
            ConnectionRefused,
            NetworkDown,
            NetworkUnreachable,
            HostUnreachable,
            OutOfMemory,
            UnexpectedEof,
        ] {
            assert!(transient_io(&io::Error::from(kind)), "{kind:?}");
        }
        assert!(transient_io(&io::Error::from_raw_os_error(ENOBUFS)));
        // As the kernel numbers them, and as std reads each number.
        #[cfg(target_os = "linux")]
        for errno in [105, 12, 104, 103, 32, 110, 111, 100, 101, 113] {
            assert!(
                transient_io(&io::Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
        #[cfg(target_os = "macos")]
        for errno in [55, 12, 54, 53, 32, 60, 61, 50, 51, 65] {
            assert!(
                transient_io(&io::Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
        #[cfg(windows)]
        for errno in [10055, 10054, 10053, 10060, 10061, 10050, 10051, 10065] {
            assert!(
                transient_io(&io::Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
        for kind in [InvalidData, PermissionDenied, NotFound, StorageFull] {
            assert!(!transient_io(&io::Error::from(kind)), "{kind:?}");
        }
        assert!(transient(&ureq::Error::Timeout(Timeout::Connect)));
        assert!(transient(&ureq::Error::Timeout(Timeout::SendBody)));
        assert!(transient(&ureq::Error::HostNotFound));
        assert!(transient(&ureq::Error::Other(Box::new(LookupFailed(
            ureq::Error::HostNotFound
        )))));
        // The server is there, and has not answered in ten minutes.
        assert!(!transient(&ureq::Error::Timeout(Timeout::RecvResponse)));
        assert!(!transient(&ureq::Error::BodyExceedsLimit(1)));
        assert!(!transient(&ureq::Error::Other("anything else".into())));
        for status in [502, 503, 504] {
            assert!(transient_status(status), "{status}");
        }
        for status in [200, 400, 401, 404, 409, 413, 429, 500, 501, 505, 507] {
            assert!(!transient_status(status), "{status}");
        }
    }

    #[test]
    fn the_waits_come_to_about_a_minute_in_a_sync_and_seconds_in_a_command() {
        let half = Duration::from_millis(500);
        let said =
            |r: Retries| -> Vec<String> { r.waits().iter().map(|n| seconds(half * *n)).collect() };
        assert_eq!(
            said(Retries::Sync),
            ["0.5 s", "1 s", "2 s", "4 s", "8 s", "16 s", "30 s"]
        );
        assert_eq!(said(Retries::Short), ["0.5 s", "1 s", "2 s"]);
        assert!(said(Retries::Never).is_empty());
        let total = |r: Retries| half * r.waits().iter().sum::<u32>();
        assert_eq!(total(Retries::Sync), Duration::from_millis(61_500));
        assert_eq!(total(Retries::Short), Duration::from_millis(3_500));
    }

    /// A host that drops every packet costs 30 s a connect: the request stops at about 3 minutes, its
    /// tries and waits together, not past 5.
    #[test]
    fn a_request_stops_trying_at_about_three_minutes() {
        let waits = Retries::Sync.waits();
        let half = Duration::from_millis(500);
        let mut elapsed = Duration::ZERO;
        let mut tries = 0;
        while let Some(wait) = next_wait(waits, tries, elapsed, half, MAX_REQUEST_TIME) {
            // Each try a connect that times out.
            elapsed += wait + Duration::from_secs(30);
            tries += 1;
        }
        assert!(tries < waits.len(), "{tries}");
        assert!(
            elapsed <= MAX_REQUEST_TIME + Duration::from_secs(30),
            "{elapsed:?}"
        );
        // A quick failure uses the whole budget.
        let quick: Vec<_> = (0..waits.len())
            .map(|t| next_wait(waits, t, Duration::from_secs(1), half, MAX_REQUEST_TIME))
            .collect();
        assert!(quick.iter().all(Option::is_some));
        assert_eq!(
            next_wait(waits, waits.len(), Duration::ZERO, half, MAX_REQUEST_TIME),
            None
        );
    }

    #[test]
    fn download_refuses_a_valid_chunk_that_was_not_asked_for() {
        let mut frames = Vec::new();
        wire::write_frame(&mut frames, &Hash::of(b"other"), b"other");
        let client = Client::new(
            &Config {
                server: hostile_server(frames),
                namespace: "default".into(),
                token: "t".into(),
                device: cairn_protocol::DeviceId::random(),
                device_name: "d".into(),
                policy: Default::default(),
                filenames: Default::default(),
                allow_http: false,
            },
            Retries::Short,
        )
        .unwrap();
        let err = client.download(&[Hash::of(b"wanted")]).unwrap_err();
        assert!(err.to_string().contains("asked for"), "{err}");
    }

    #[test]
    fn a_valid_chunk_that_was_not_asked_for_is_refused() {
        let (a, b) = (Hash::of(b"a"), Hash::of(b"b"));
        assert!(check_answer(&[a, b], &[(a, b"a"), (b, b"b")]).is_ok());
        assert!(check_answer(&[a, b], &[(b, b"b"), (a, b"a")]).is_err());
        assert!(check_answer(&[a], &[(b, b"b")]).is_err());
        assert!(check_answer(&[a, b], &[(a, b"a")]).is_err());
    }

    #[derive(Debug, Default)]
    struct Counting(std::sync::atomic::AtomicUsize);

    impl Resolver for Counting {
        fn resolve(
            &self,
            uri: &ureq::http::Uri,
            config: &ureq::config::Config,
            timeout: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            DefaultResolver::default().resolve(uri, config, timeout)
        }
    }

    #[test]
    fn the_server_name_is_looked_up_once_not_per_request() {
        let r = CachedResolver {
            inner: Counting::default(),
            last: Mutex::new(None),
        };
        let config = ureq::config::Config::default();
        let a: ureq::http::Uri = "http://localhost:1/v1/info".parse().unwrap();
        let b: ureq::http::Uri = "http://localhost:2/v1/info".parse().unwrap();
        for _ in 0..5 {
            r.resolve(&a, &config, NextTimeout::default()).unwrap();
        }
        assert_eq!(r.inner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Another server is another name.
        r.resolve(&b, &config, NextTimeout::default()).unwrap();
        assert_eq!(r.inner.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        // And an old answer is asked again.
        r.last.lock().unwrap().as_mut().unwrap().1 -= NAME_TTL;
        r.resolve(&b, &config, NextTimeout::default()).unwrap();
        assert_eq!(r.inner.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn only_loopback_hosts_count_as_loopback() {
        for yes in [
            "http://127.0.0.1:1",
            "http://localhost:80/x",
            "http://[::1]:1",
            "http://127.1.2.3",
        ] {
            assert!(host_is_loopback(yes), "{yes}");
        }
        for no in [
            "http://example.org",
            "http://127.0.0.1.example.org",
            "http://10.0.0.1:1",
            "http://localhost.example.org",
        ] {
            assert!(!host_is_loopback(no), "{no}");
        }
    }
}
