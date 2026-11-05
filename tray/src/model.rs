//! What the tray says of a root: its light, a line for the menu, and its problems, each the state and
//! what to do about it. Shared by the menu, the window and `--probe`, so all three say the same.

use crate::hub::{Link, RootView};
use crate::link::LinkError;
use cairn_protocol::local::{Change, DaemonStatus, Secs};
use cairn_protocol::SkipKind;
use serde::Serialize;

/// Better to worse: the tray shows the worst of all roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Light {
    Synced,
    Syncing,
    Waiting,
    Paused,
    Error,
    NotRunning,
}

impl Light {
    pub fn word(self) -> &'static str {
        match self {
            Light::Synced => "synced",
            Light::Syncing => "syncing",
            Light::Waiting => "waiting",
            Light::Paused => "paused",
            Light::Error => "needs attention",
            Light::NotRunning => "not running",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub what: String,
    pub todo: String,
}

pub fn now_secs() -> Secs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as Secs)
}

/// `16:52` today, `3 Nov 16:52` before.
pub fn when(secs: Secs) -> String {
    use chrono::{Local, TimeZone};
    let Some(t) = Local.timestamp_opt(secs, 0).single() else {
        return format!("{secs}");
    };
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%-d %b %H:%M").to_string()
    }
}

pub fn light(view: &RootView, _now: Secs) -> Light {
    let Link::Up(s) = &view.link else {
        return Light::NotRunning;
    };
    if s.state == "error" || s.feed_down.is_some() || paused_itself(s) {
        return Light::Error;
    }
    if s.paused_since.is_some() {
        return Light::Paused;
    }
    if s.syncing || s.state.is_empty() {
        return Light::Syncing;
    }
    if s.state == "partial" || s.waiting.is_some() || s.unwatched > 0 {
        return Light::Waiting;
    }
    Light::Synced
}

/// Paused by the daemon itself, which waits for a person: a missing mount, say.
fn paused_itself(s: &DaemonStatus) -> bool {
    s.state == "paused" && s.paused_since.is_none()
}

pub fn name(view: &RootView) -> String {
    view.entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| view.entry.path.display().to_string())
}

/// One line on what the daemon of this root is doing.
pub fn headline(view: &RootView, now: Secs) -> String {
    let s = match &view.link {
        Link::Connecting => return "connecting to its daemon".into(),
        Link::Down { since, why } => {
            return match why {
                LinkError::NotRunning(_) => format!("not running, since {}", when(*since)),
                LinkError::Refused(w) | LinkError::Other(w) => format!("not reached: {w}"),
            }
        }
        Link::Up(s) => s,
    };
    if let Some(why) = &s.feed_down {
        return format!("cannot reach the server: {why}");
    }
    let detail = |d: &Option<String>, or: &str| d.clone().unwrap_or_else(|| or.to_string());
    match s.state.as_str() {
        "" => "starting".into(),
        "error" => format!("sync failed: {}", detail(&s.detail, "see its log")),
        "paused" => match s.paused_since {
            Some(t) => format!("paused by you since {}", when(t)),
            None => format!("paused: {}", detail(&s.detail, "it waits for a person")),
        },
        _ if light(view, now) == Light::Syncing => "syncing".into(),
        "partial" => format!("partly synced: {}", detail(&s.detail, "some changes wait")),
        _ => match &s.waiting {
            Some(w) => format!("changes wait until {}: {}", when(w.until), w.why),
            None if s.last_synced == 0 => "not fully synced yet".into(),
            None => format!("synced at {}", when(s.last_synced)),
        },
    }
}

/// The action a skip's text calls for, found by its reason.
fn skip_action(text: &str) -> &'static str {
    const KINDS: [SkipKind; 14] = [
        SkipKind::Fifo,
        SkipKind::Socket,
        SkipKind::CharDevice,
        SkipKind::BlockDevice,
        SkipKind::MountPoint,
        SkipKind::PermissionDenied,
        SkipKind::KeepsChanging,
        SkipKind::VanishedDuringScan,
        SkipKind::CannotApply,
        SkipKind::CannotHoldName,
        SkipKind::ExcludedInTheWay,
        SkipKind::ReadError,
        SkipKind::Unsupported,
        SkipKind::SameNameOnServer,
    ];
    KINDS
        .iter()
        .find(|k| text.contains(&format!(": {}", k.reason())))
        .map_or("", |k| k.action())
}

pub fn problems(view: &RootView) -> Vec<Problem> {
    let p = |what: String, todo: &str| Problem {
        what,
        todo: todo.to_string(),
    };
    let s = match &view.link {
        Link::Connecting => return Vec::new(),
        Link::Down { since, why } => {
            let mut out = Vec::new();
            if let Some(gone) = gone(view) {
                out.push(gone);
            }
            out.push(match why {
                LinkError::NotRunning(_) => p(
                    format!("No daemon answers for this folder, since {}", when(*since)),
                    "Its changes are kept and wait until one runs: start the service that runs `cairn daemon` here, or run it.",
                ),
                LinkError::Refused(w) => p(
                    format!("Its daemon's address is not to be trusted: {w}"),
                    "Find what holds it: this user's daemon cannot answer there while it does.",
                ),
                LinkError::Other(w) => p(
                    format!("Its daemon cannot be reached: {w}"),
                    "The tray tries again by itself; if it stays, the daemon's log says more.",
                ),
            });
            return out;
        }
        Link::Up(s) => s,
    };
    let mut out = Vec::new();
    match s.state.as_str() {
        "error" => out.push(p(
            format!("Sync failed: {}", s.detail.as_deref().unwrap_or("no reason given")),
            "The daemon tries again, waiting up to a minute between tries; if it keeps failing, its log says more.",
        )),
        "paused" if s.paused_since.is_none() => out.push(p(
            format!("Sync paused itself: {}", s.detail.as_deref().unwrap_or("no reason given")),
            "It waits for a person: deal with the reason, and it goes on; `cairn status` in the folder says what it would do.",
        )),
        "partial" => out.push(p(
            format!(
                "Some changes did not sync: {}",
                s.detail.as_deref().unwrap_or("see below")
            ),
            "The daemon retries them; below is why each waits.",
        )),
        _ => {}
    }
    if let Some(t) = s.paused_since {
        out.push(p(
            format!("Paused by you since {}", when(t)),
            "Resume from the menu: what changed meanwhile is kept, and syncs then.",
        ));
    }
    if let Some(why) = &s.feed_down {
        out.push(p(
            format!("The server cannot be reached: {why}"),
            "Changes here are kept and wait, and go up once it answers: check the network or the server.",
        ));
    }
    if let Some(w) = &s.waiting {
        out.push(p(
            format!("Changes wait until {}: {}", when(w.until), w.why),
            "Nothing to do: the daemon goes on by itself then.",
        ));
    }
    if s.unwatched > 0 {
        out.push(p(
            format!(
                "{} folders are not watched, so changes in them wait for a full sync",
                s.unwatched
            ),
            if s.watch_limit_reached {
                "Raise fs.inotify.max_user_watches."
            } else {
                "The full sync the daemon runs regularly finds them."
            },
        ));
    }
    for skip in &s.skips {
        out.push(p(skip.clone(), skip_action(skip)));
    }
    for n in &s.not_applied {
        out.push(p(
            format!(
                "Not applied here: {}: {} (since {})",
                n.path,
                n.text,
                when(n.since)
            ),
            "The server keeps it, so nothing is lost; this filesystem refused it here.",
        ));
    }
    let more = s.not_applied_count.saturating_sub(s.not_applied.len());
    if more > 0 {
        out.push(p(
            format!("And {more} more not applied here"),
            "`cairn status` in the folder lists them.",
        ));
    }
    out
}

/// Shown as gone, with Forget offered: no daemon answers, and the folder does not hold the root.
pub fn shown_gone(view: &RootView) -> bool {
    matches!(view.link, Link::Down { .. }) && gone(view).is_some()
}

/// Whether the root itself has gone from where the registry says it is: unmounted, or replaced.
fn gone(view: &RootView) -> Option<Problem> {
    let todo = "If its disk is not mounted, mount it: a daemon pauses rather than taking a missing folder for deletes. If it is gone for good, Forget takes it off this list.";
    match crate::registry::root_id_at(&view.entry.path) {
        Ok(id) if id == view.entry.root_id => None,
        Ok(_) => Some(Problem {
            what: format!("Another root is at {} now", view.entry.path.display()),
            todo: "This one may have moved: `cairn status` in the folder says which it is. If it is gone for good, Forget takes it off this list.".into(),
        }),
        Err(_) => Some(Problem {
            what: format!(
                "This folder is not there, or not a root: {}",
                view.entry.path.display()
            ),
            todo: todo.into(),
        }),
    }
}

pub fn change_word(c: Change) -> &'static str {
    match c {
        Change::Added => "added",
        Change::Changed => "changed",
        Change::Metadata => "metadata changed",
        Change::Moved => "moved",
        Change::Removed => "removed",
        Change::Conflict => "conflict copy made",
    }
}

pub fn open_conflicts(view: &RootView) -> Option<usize> {
    match &view.conflicts {
        Some((_, Ok(list))) => Some(list.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_finds_its_action_by_its_reason() {
        let s = format!("a/b: {}", SkipKind::CannotHoldName.reason());
        assert_eq!(skip_action(&s), SkipKind::CannotHoldName.action());
        assert_eq!(skip_action("a: something new"), "");
    }

    /// Forget is offered only where no daemon answers and the folder does not hold the root.
    #[test]
    fn only_a_root_without_its_daemon_or_its_folder_is_shown_gone() {
        use cairn_protocol::local::RootEntry;
        let dir = std::env::temp_dir().join(format!("cairn-tray-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("here/.cairn")).unwrap();
        std::fs::write(dir.join("here/.cairn/root-id"), "id-here\n").unwrap();
        let view = |name: &str, id: &str, link: Link| {
            let entry = RootEntry {
                path: dir.join(name),
                root_id: id.into(),
                server: String::new(),
                added: 0,
            };
            RootView::detached(entry, link, None)
        };
        let down = || Link::Down {
            since: 0,
            why: LinkError::NotRunning("no daemon answers".into()),
        };
        assert!(shown_gone(&view("gone", "id-gone", down())));
        // Another root is there now.
        assert!(shown_gone(&view("here", "id-gone", down())));
        assert!(!shown_gone(&view("here", "id-here", down())));
        assert!(!shown_gone(&view("gone", "id-gone", Link::Connecting)));
        assert!(!shown_gone(&view(
            "gone",
            "id-gone",
            Link::Up(Box::default())
        )));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_worst_light_is_not_running() {
        assert!(Light::NotRunning > Light::Error && Light::Error > Light::Paused);
        assert!(Light::Paused > Light::Waiting && Light::Syncing > Light::Synced);
    }
}
