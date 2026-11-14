//! The phone apps' core as a process the harness can kill at its failpoints, as iOS ends an app or an
//! extension at any moment:
//!
//! - `h-mobile upload <db> <server> <token-file> <folder> <base> <file> <asset> <original> <created>`
//!   queues one photo library resource, as the app does, and sends it from `file`;
//! - `h-mobile download <server> <token-file> <folder> <object> <revision> <dest> <size>`.
//!
//! Each prints what it ended with, as JSON, and exits 0, or 1 with the error.

use cairn_mobile::naming::NameStyle;
use cairn_mobile::uploads::{AssetResource, Destination, UploadState};
use cairn_mobile::{new_device_id, Session, Store};
use std::process::ExitCode;

fn session(
    db: Option<&Store>,
    server: &str,
    token_file: &str,
) -> Result<std::sync::Arc<Session>, String> {
    let token = std::fs::read_to_string(token_file).map_err(|e| format!("{token_file}: {e}"))?;
    // The device's ID, made once, as the app keeps it.
    let device = match db {
        Some(db) => match db.get_text("device".into()).map_err(|e| e.to_string())? {
            Some(d) => d,
            None => {
                let d = new_device_id();
                db.set_text("device".into(), Some(d.clone()))
                    .map_err(|e| e.to_string())?;
                d
            }
        },
        None => new_device_id(),
    };
    Session::new(
        server.into(),
        token,
        device,
        "h-mobile iPhone".into(),
        false,
    )
    .map_err(|e| e.to_string())
}

fn run(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("upload") if args.len() == 10 => {
            let [_, db, server, token, folder, base, file, asset, original, created] =
                <[String; 10]>::try_from(args.to_vec()).expect("ten arguments");
            let store = Store::open(db).map_err(|e| e.to_string())?;
            let s = session(Some(&store), &server, &token)?;
            let f = s.folder(folder.clone()).map_err(|e| e.to_string())?;
            let planned = store
                .plan(
                    Destination {
                        folder,
                        path: base,
                        style: NameStyle::Nextcloud,
                        subfolders: false,
                    },
                    vec![AssetResource {
                        asset,
                        kind: 1,
                        idx: 0,
                        original_name: original,
                        created_sec: created.parse().map_err(|_| "a bad time".to_string())?,
                        created_nsec: 0,
                        utc_offset: 0,
                    }],
                )
                .map_err(|e| e.to_string())?;
            let u = &planned[0];
            let done = if u.state == UploadState::Done {
                u.clone()
            } else {
                store
                    .upload_now(u.id, f, file, None)
                    .map_err(|e| e.to_string())?
            };
            Ok(format!(
                "{{\"state\":\"{:?}\",\"path\":{:?},\"name\":{:?},\"object\":{:?},\"error\":{:?}}}",
                done.state,
                done.path,
                done.name.unwrap_or_default(),
                done.done_object.unwrap_or_default(),
                done.error.unwrap_or_default()
            ))
        }
        Some("download") if args.len() == 8 => {
            let [_, server, token, folder, object, revision, dest, size] =
                <[String; 8]>::try_from(args.to_vec()).expect("eight arguments");
            let s = session(None, &server, &token)?;
            let f = s.folder(folder).map_err(|e| e.to_string())?;
            let n = f
                .download(
                    object,
                    revision,
                    dest,
                    size.parse().map_err(|_| "a bad size".to_string())?,
                    None,
                )
                .map_err(|e| e.to_string())?;
            Ok(format!("{{\"bytes\":{n}}}"))
        }
        _ => Err("usage: h-mobile upload … | download …".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("h-mobile: {e}");
            ExitCode::FAILURE
        }
    }
}
