//! The HTTP client. Every call is safe to retry; the client does not retry on its own, since a
//! later sync picks up from durable state.

use crate::root::Config;
use anyhow::{anyhow, bail, Context, Result};
use cairn_protocol::wire::{
    self, ChangesResponse, CommitRequest, CommitResponse, ConflictEntry, HashList, Info,
};
use cairn_protocol::Hash;
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::Agent;

/// The server said its disk is full; nothing there changed.
#[derive(Debug, thiserror::Error)]
#[error("the server's disk is full: nothing was lost; free space on the server and sync again")]
pub struct ServerFull;

pub struct Client {
    agent: Agent,
    base: String,
    auth: String,
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
    pub fn new(config: &Config) -> Result<Self> {
        let url = &config.server;
        if url.starts_with("http://") {
            if !host_is_loopback(url) && !config.allow_http {
                bail!("{url} is plain HTTP beyond loopback, which sends the token in the clear: use https://, or set allow_http");
            }
        } else if !url.starts_with("https://") {
            bail!("{url} is not an http:// or https:// URL");
        }
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        let client = Self {
            agent,
            base: format!("{url}/v1/ns/{}", config.namespace),
            auth: format!("Bearer {}", config.token),
        };
        let info = client.info()?;
        if info.protocol != cairn_protocol::PROTOCOL_VERSION {
            bail!(
                "the server speaks protocol {}, this client {}: upgrade whichever is older",
                info.protocol,
                cairn_protocol::PROTOCOL_VERSION
            );
        }
        Ok(client)
    }

    fn read(&self, what: &str, mut resp: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>> {
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_vec()
            .with_context(|| format!("{what}: reading the response"))?;
        match status {
            200..=299 => Ok(body),
            401 => bail!("{what}: the server refused the access token"),
            507 => Err(anyhow!(ServerFull)),
            other => bail!(
                "{what}: server answered {other}: {}",
                String::from_utf8_lossy(&body)
            ),
        }
    }

    fn get_json<T: DeserializeOwned>(&self, what: &str, path: &str) -> Result<T> {
        let resp = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .call()
            .with_context(|| format!("{what}: cannot reach the server"))?;
        Ok(serde_json::from_slice(&self.read(what, resp)?)?)
    }

    fn post_bytes(
        &self,
        what: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let resp = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .header("Content-Type", content_type)
            .send(body)
            .with_context(|| format!("{what}: cannot reach the server"))?;
        self.read(what, resp)
    }

    fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        what: &str,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let bytes = self.post_bytes(what, path, "application/json", &serde_json::to_vec(body)?)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn info(&self) -> Result<Info> {
        self.get_json("server info", "/info")
    }

    pub fn missing(&self, hashes: &[Hash]) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        for batch in hashes.chunks(4096) {
            let resp: HashList = self.post_json(
                "missing chunks",
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
            "/chunks/download",
            "application/json",
            &serde_json::to_vec(&HashList {
                hashes: hashes.to_vec(),
            })?,
        )?;
        let frames = wire::read_frames(&body).context("downloading chunks")?;
        check_answer(hashes, &frames)?;
        Ok(frames.into_iter().map(|(h, d)| (h, d.to_vec())).collect())
    }

    pub fn commit(&self, req: &CommitRequest) -> Result<CommitResponse> {
        self.post_json("committing", "/commit", req)
    }

    pub fn changes(&self, after: u64) -> Result<ChangesResponse> {
        self.get_json(
            "reading changes",
            &format!("/changes?after={after}&limit=1000"),
        )
    }

    /// The head, once it is not `after` or `wait` has gone by. A plain request and answer, so it
    /// passes through any reverse proxy. Bounded here too: a connection a firewall silently dropped
    /// answers nothing, ever.
    pub fn wait_for_head(&self, after: u64, wait: Duration) -> Result<u64> {
        let what = "waiting for changes";
        let resp = self
            .agent
            .get(format!(
                "{}/changes?after={after}&limit=0&wait={}",
                self.base,
                wait.as_secs()
            ))
            .config()
            .timeout_recv_response(Some(wait + Duration::from_secs(15)))
            .build()
            .header("Authorization", &self.auth)
            .call()
            .with_context(|| format!("{what}: cannot reach the server"))?;
        let answer: ChangesResponse = serde_json::from_slice(&self.read(what, resp)?)?;
        Ok(answer.head)
    }

    pub fn conflicts(&self) -> Result<Vec<ConflictEntry>> {
        self.get_json("reading conflicts", "/conflicts")
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

    #[test]
    fn download_refuses_a_valid_chunk_that_was_not_asked_for() {
        let mut frames = Vec::new();
        wire::write_frame(&mut frames, &Hash::of(b"other"), b"other");
        let client = Client::new(&Config {
            server: hostile_server(frames),
            namespace: "default".into(),
            token: "t".into(),
            device: cairn_protocol::DeviceId::random(),
            device_name: "d".into(),
            policy: Default::default(),
            filenames: Default::default(),
            allow_http: false,
        })
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
