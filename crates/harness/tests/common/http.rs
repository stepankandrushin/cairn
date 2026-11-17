//! Plain HTTP/1.1 requests to a test server, as a browser or any client makes them, with a token, a
//! session cookie or nothing.

use super::Server;
use serde_json::Value;
use std::io::{Read, Write};

#[derive(Clone, Copy)]
pub enum Auth<'a> {
    None,
    Bearer(&'a str),
    /// The session cookie, and whether the web's header comes with it.
    Cookie(&'a str, bool),
}

pub struct Answer {
    pub code: u16,
    pub head: String,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    #[track_caller]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {} {}\n{}", self.code, self.head, self.text()))
    }

    pub fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_lowercase());
        self.head
            .lines()
            .find_map(|l| l.strip_prefix(&want).map(|v| v.trim().to_string()))
    }

    #[track_caller]
    pub fn ok(self) -> Self {
        assert!(
            (200..300).contains(&self.code),
            "{} {}\n{}",
            self.code,
            self.head,
            self.text()
        );
        self
    }
}

/// One HTTP/1.1 request, read to the end, a chunked body put together.
pub fn request(
    server: &Server,
    method: &str,
    path: &str,
    auth: Auth,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Answer {
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    match auth {
        Auth::None => {}
        Auth::Bearer(t) => head.push_str(&format!("Authorization: Bearer {t}\r\n")),
        Auth::Cookie(t, header) => {
            head.push_str(&format!("Cookie: other=1; cairn={t}\r\n"));
            if header {
                head.push_str("X-Cairn-Web: 1\r\n");
            }
        }
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes()).unwrap();
    // A server may answer before it reads the body, and close: the answer is still there to read.
    let _ = conn.write_all(body);
    let mut raw = Vec::new();
    let _ = conn.read_to_end(&mut raw);
    // Interim answers before the answer, as a resumable upload's 104: passed over, as a client does.
    while raw.starts_with(b"HTTP/1.1 1") && !raw.starts_with(b"HTTP/1.1 101") {
        let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            break;
        };
        raw.drain(..end + 4);
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("an answer has a head");
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let mut body = raw[split + 4..].to_vec();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    if head.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut rest = &body[..];
        loop {
            let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else {
                panic!("a chunked body ended early: the server cut it off");
            };
            let len = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap().trim(), 16)
                .unwrap();
            rest = &rest[eol + 2..];
            if len == 0 {
                break;
            }
            assert!(
                rest.len() >= len + 2,
                "a chunked body ended early: the server cut it off"
            );
            out.extend_from_slice(&rest[..len]);
            rest = &rest[len + 2..];
        }
        body = out;
    }
    Answer { code, head, body }
}

pub fn get(server: &Server, path: &str, auth: Auth) -> Answer {
    request(server, "GET", path, auth, &[], b"")
}

pub fn post(server: &Server, path: &str, auth: Auth, body: Value) -> Answer {
    request(
        server,
        "POST",
        path,
        auth,
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    )
}
