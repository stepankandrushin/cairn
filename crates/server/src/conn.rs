//! The server's accept loop, in place of `axum::serve` and doing as it does (HTTP/1.1 by hyper on each
//! connection, accept errors waited out, a graceful shutdown that lets each connection finish), with one
//! thing more: each connection's socket is shared with the handlers of its requests, through the
//! [`Interim`] extension, so that a handler can send an informational answer while it reads the body.
//! That is the 104 of a resumable upload (docs/protocol.md, "One-request uploads"), which hyper 1 does
//! not send: it sends no 1xx but its own 100 Continue.
//!
//! Safe because HTTP/1.1 answers one request at a time, and hyper writes nothing of a request's answer
//! before its handler returns: what the handler writes before then is a whole answer of its own, between
//! the last answer and this one. The only bytes hyper may write meanwhile are its 100 Continue, when the
//! body is first read; a handler sends its own after the first part of the body has come, by when the
//! 100 Continue is written.

use axum::body::Body;
use axum::extract::Request;
use axum::Router;
use std::future::Future;
use std::io;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// A request's connection, to write an informational answer to before the handler's own.
#[derive(Clone)]
pub(crate) struct Interim(Arc<TcpStream>);

impl Interim {
    /// An informational answer, `status` 102 to 199, with these headers, written whole.
    pub(crate) async fn send(
        &self,
        status: u16,
        reason: &str,
        headers: &[(&str, &str)],
    ) -> io::Result<()> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());
        if !(102..200).contains(&status) {
            return Err(bad("an informational status is 102 to 199"));
        }
        let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
        for (name, value) in headers {
            if name.is_empty()
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
            {
                return Err(bad("a header that cannot be sent as it is"));
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut buf = head.as_bytes();
        while !buf.is_empty() {
            self.0.writable().await?;
            match self.0.try_write(buf) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// One connection's socket as hyper reads and writes it: through `&TcpStream`, which [`Interim`]
/// writes through too.
struct Shared(Arc<TcpStream>);

impl AsyncRead for Shared {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            ready!(self.0.poll_read_ready(cx))?;
            match self.0.try_read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for Shared {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.0.poll_write_ready(cx))?;
            match self.0.try_write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                done => return Poll::Ready(done),
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.0.poll_write_ready(cx))?;
            match self.0.try_write_vectored(bufs) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                done => return Poll::Ready(done),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(socket2::SockRef::from(&*self.0).shutdown(std::net::Shutdown::Write))
    }
}

/// Serves `router` on `listener` until `signal`, then waits for every connection to finish what it
/// was answering.
pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    signal: impl Future<Output = ()> + Send + 'static,
) {
    let (signal_tx, signal_rx) = watch::channel(());
    tokio::spawn(async move {
        signal.await;
        drop(signal_rx);
    });
    let (close_tx, close_rx) = watch::channel(());
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // As axum does: a connection's own error is that connection's; anything else (out of
                    // file descriptors) is waited out.
                    if !is_connection_error(&e) {
                        eprintln!("cairn-server: accept: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            },
            _ = signal_tx.closed() => break,
        };
        let stream = Arc::new(stream);
        let interim = Interim(stream.clone());
        let router = router.clone();
        let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
            let mut req = req.map(Body::new);
            req.extensions_mut().insert(interim.clone());
            tower::ServiceExt::oneshot(router.clone(), req)
        });
        let signal_tx = signal_tx.clone();
        let close_rx = close_rx.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(Shared(stream));
            let mut conn = pin!(hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades());
            tokio::select! {
                _ = conn.as_mut() => {}
                _ = signal_tx.closed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.as_mut().await;
                }
            }
            drop(close_rx);
        });
    }
    drop(close_rx);
    drop(listener);
    close_tx.closed().await;
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}
