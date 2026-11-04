//! Runs what the tray's menu and window run, from a shell: for trying them over SSH against
//! `mock-daemon`, where nobody can click. The same source files as the tray, not a copy.
//!
//!     drive <root_id> tell sync_now|pause|resume
//!     drive <root_id> conflict <seq> show|open_both|keep_original|keep_copy
#![allow(dead_code)]

#[path = "../src/actions.rs"]
mod actions;
#[path = "../src/hub.rs"]
mod hub;
#[path = "../src/link.rs"]
mod link;
#[path = "../src/model.rs"]
mod model;

use cairn_protocol::local::Request;
use std::process::ExitCode;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let r = match args.as_slice() {
        [root, "tell", what] => {
            let request = match *what {
                "sync_now" => Request::SyncNow,
                "pause" => Request::Pause,
                "resume" => Request::Resume,
                _ => return usage(),
            };
            link::tell(root, request).map(|()| "done".to_string())
        }
        [root, "conflict", seq, what] => {
            let (Ok(seq), Some(action)) = (seq.parse(), actions::ConflictAction::parse(what))
            else {
                return usage();
            };
            let hub = hub::Hub::start();
            let start = Instant::now();
            while hub.root(root).is_none() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(100));
            }
            actions::conflict(&hub, root, seq, action)
        }
        _ => return usage(),
    };
    match r {
        Ok(msg) => {
            println!("ok: {msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: drive ROOT_ID tell sync_now|pause|resume | drive ROOT_ID conflict SEQ ACTION"
    );
    ExitCode::from(64)
}
