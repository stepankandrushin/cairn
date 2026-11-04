//! `cairn-tray`: shows what each of this user's Cairn daemons is doing, and opens what a person deals
//! with. It is a program nobody needs: the daemons run without it, and it holds no sync logic.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// Without its window, what only the window uses is left unused: that build is for `--probe`.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

mod actions;
#[cfg(feature = "gui")]
mod gui;
mod hub;
mod icon;
mod link;
mod model;

use cairn_protocol::local::{registry_path, user_dir, Direction, Reply, Request};
use hub::{Link, RootView};
use link::LinkError;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

/// A line on stdout; once nobody reads it, as in `cairn-tray --probe | head`, the probe stops.
macro_rules! say {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    };
}

const USAGE: &str =
    "usage: cairn-tray [--open TAB | --probe [--conflicts] | --login on|off | --version]
  (none)       the tray icon, its menu and window
  --open TAB   the same, with its window open at status, recent, problems or conflicts
  --probe      print, with no window, what the tray would show for each root; exit 2 if the
               registry of roots cannot be read. With --conflicts, the open conflicts too, which
               has the server replay its journal
  --login on   start the tray at login (off: no longer)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--probe"] => probe(false),
        ["--probe", "--conflicts"] => probe(true),
        ["--login", on @ ("on" | "off")] => match actions::login::set(*on == "on") {
            Ok(()) => {
                println!("start at login: {on}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("cairn-tray: {e}");
                ExitCode::FAILURE
            }
        },
        ["--version"] => {
            println!("cairn-tray {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        [] => tray(None),
        ["--open", tab @ ("status" | "recent" | "problems" | "conflicts")] => tray(Some(tab)),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(64)
        }
    }
}

#[cfg(feature = "gui")]
fn tray(open: Option<&str>) -> ExitCode {
    // One tray per user: a second start finds the first holding this.
    let _lock = match single() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log("another cairn-tray of this user is running: leaving it be");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            log(&format!(
                "cannot take the tray's lock, starting anyway: {e}"
            ));
            None
        }
    };
    log(&format!(
        "cairn-tray {} starting",
        env!("CARGO_PKG_VERSION")
    ));
    match gui::run(hub::Hub::start(), open.map(str::to_string)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(&e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "gui"))]
fn tray(_: Option<&str>) -> ExitCode {
    eprintln!("cairn-tray: built without its window, so only --probe and --login\n{USAGE}");
    ExitCode::from(64)
}

/// `Ok(None)` when another holds it.
#[cfg(feature = "gui")]
fn single() -> std::io::Result<Option<Option<std::fs::File>>> {
    let dir = user_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("tray.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Some(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// A line in `tray.log` in the user's Cairn directory, and on stderr: on Windows the tray has no
/// console, so the file is the only place a failure can be read later.
pub fn log(line: &str) {
    eprintln!("cairn-tray: {line}");
    let Some(dir) = user_dir() else { return };
    let path = dir.join("tray.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&path, dir.join("tray.log.old"));
    }
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let t = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "{t} {line}");
    }
}

fn probe(with_conflicts: bool) -> ExitCode {
    let Some(path) = registry_path() else {
        say!("registry: no home directory to find it in");
        return ExitCode::from(2);
    };
    let registry = match hub::read_registry(&path) {
        Ok(r) => r,
        Err(e) => {
            say!("registry: {e}");
            return ExitCode::from(2);
        }
    };
    say!(
        "registry: {}: {} root{}",
        path.display(),
        registry.roots.len(),
        if registry.roots.len() == 1 { "" } else { "s" }
    );
    let now = model::now_secs();
    let mut worst = None;
    for entry in registry.roots {
        let link = match link::ask(&entry.root_id, Request::Status, Duration::from_secs(5)) {
            Ok(Reply::Status(s)) => Link::Up(s),
            Ok(Reply::Error { text }) => Link::Down {
                since: now,
                why: LinkError::Other(text),
            },
            Ok(other) => Link::Down {
                since: now,
                why: LinkError::Other(format!("the daemon answered {other:?}")),
            },
            Err(why) => Link::Down { since: now, why },
        };
        let conflicts = (with_conflicts && matches!(link, Link::Up(_))).then(|| {
            match link::ask(&entry.root_id, Request::Conflicts, Duration::from_secs(60)) {
                Ok(Reply::Conflicts { open }) => Ok(open),
                Ok(Reply::Error { text }) => Err(text),
                Ok(other) => Err(format!("the daemon answered {other:?}")),
                Err(e) => Err(e.to_string()),
            }
        });
        let view = RootView::detached(entry, link, conflicts);
        print_root(&view, now);
        let l = model::light(&view, now);
        worst = worst.max(Some(l));
    }
    let worst = worst.unwrap_or(model::Light::NotRunning);
    say!("tray icon: {}", worst.word());
    ExitCode::SUCCESS
}

fn print_root(view: &RootView, now: i64) {
    let e = &view.entry;
    say!("\n{}", e.path.display());
    say!("  root {}, server {}", e.root_id, e.server);
    say!(
        "  {}: {}",
        model::light(view, now).word(),
        model::headline(view, now)
    );
    if let Link::Up(s) = &view.link {
        say!(
            "  daemon {} (process {}), at sequence {}, {} syncs, {} folders watched",
            if s.version.is_empty() {
                "?"
            } else {
                &s.version
            },
            s.pid,
            s.head,
            s.syncs,
            s.watched
        );
        if s.recent.is_empty() {
            say!("  recent: nothing yet");
        } else {
            say!("  recent, newest first, {} kept:", s.recent.len());
            for a in s.recent.iter().take(5) {
                let to =
                    a.to.as_deref()
                        .map(|t| format!(" -> {t}"))
                        .unwrap_or_default();
                say!(
                    "    {} {} {} {}{to}",
                    model::when(a.time),
                    match a.direction {
                        Direction::Up => "sent",
                        Direction::Down => "came",
                    },
                    model::change_word(a.change),
                    a.path
                );
            }
        }
    }
    let problems = model::problems(view);
    if problems.is_empty() {
        say!("  problems: none");
    } else {
        say!("  problems:");
        for p in problems {
            say!("    - {}", p.what);
            if !p.todo.is_empty() {
                say!("      {}", p.todo);
            }
        }
    }
    match &view.conflicts {
        None => {}
        Some((_, Err(e))) => say!("  conflicts: cannot list them: {e}"),
        Some((_, Ok(list))) if list.is_empty() => say!("  conflicts: none open"),
        Some((_, Ok(list))) => {
            say!("  conflicts, {} open:", list.len());
            for c in list {
                say!(
                    "    {} {} from {}: {} against {}{}",
                    c.seq,
                    model::when(c.time),
                    c.device_name,
                    c.copy_display,
                    c.original_display
                        .as_deref()
                        .unwrap_or("what is since deleted"),
                    if c.copy.is_some() {
                        ""
                    } else {
                        " (the copy is not here)"
                    }
                );
            }
        }
    }
}
