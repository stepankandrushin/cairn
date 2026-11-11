//! The administrator's commands: users, their tokens, folders and who may do what in each. They work
//! on the data directory, while the server runs or not: with it running they touch the database
//! alone, which the server reads on every request, so a change holds from its next one.

use anyhow::{bail, Context, Result};
use cairn_protocol::wire::{valid_account_name, Access};
use cairn_store::{token_hash, token_id, Repo};
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum UserCommand {
    /// Add a user, with a first token: printed, or written to --token-file.
    Add {
        name: String,
        #[command(flatten)]
        token: NewToken,
        #[arg(long)]
        data: PathBuf,
    },
    /// Every user, with its tokens and folders.
    List {
        #[arg(long)]
        data: PathBuf,
    },
    /// Remove a user, its tokens and all its access. What it committed stays, under its name.
    Remove {
        name: String,
        #[arg(long)]
        data: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum TokenCommand {
    /// Another token for a user, for another device: printed, or written to --token-file.
    Add {
        user: String,
        #[command(flatten)]
        token: NewToken,
        #[arg(long)]
        data: PathBuf,
    },
    /// Every token by its ID, the first digits of its hash: the tokens themselves are not kept.
    List {
        #[arg(long)]
        data: PathBuf,
    },
    /// Take a token back, by the ID `token list` shows or the first digits of it. Its devices are
    /// refused from their next request on, and keep what they hold.
    Remove {
        id: String,
        #[arg(long)]
        data: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum FolderCommand {
    /// Add a folder, which nobody can reach until given access, here by --manager or by `grant`. A
    /// user adds one of their own in the web, which they manage.
    Add {
        name: String,
        /// A user to manage it; may be given more than once.
        #[arg(long = "manager")]
        managers: Vec<String>,
        #[arg(long)]
        data: PathBuf,
    },
    /// Every folder, its journal's head, its former names, and who has access at which level.
    List {
        #[arg(long)]
        data: PathBuf,
    },
    /// Rename a folder, with the server stopped, which reads the former names as it starts; while it
    /// serves, a manager of the folder renames it through it, as in the web's folder settings. Its
    /// chunks stay where they are, and its former name goes on reaching it, so that every device, link
    /// and address made with it goes on working, and devices take the new name up as they next sync;
    /// no other folder may take the former name.
    Rename {
        from: String,
        to: String,
        #[arg(long)]
        data: PathBuf,
    },
}

#[derive(clap::Args)]
pub struct NewToken {
    /// Write the token to this new file, readable by this user alone, rather than print it.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// What the token is for, such as the device that uses it, for `token list`.
    #[arg(long, default_value = "")]
    label: String,
}

fn check_name(what: &str, name: &str) -> Result<()> {
    if !valid_account_name(name) {
        bail!(
            "{name:?} cannot be a {what}'s name: 1 to 64 of letters, digits, `.`, `_` and `-`, not starting with `.` or `-`"
        );
    }
    Ok(())
}

/// A day as `2026-11-09`, UTC.
fn date(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A new token for `user`: in the file first, made new so that nothing is overwritten, then in the
/// database, so that a token that works is always one somebody has.
fn issue(repo: &Repo, user: &str, how: &NewToken) -> Result<()> {
    let token = crate::new_token();
    if let Some(path) = &how.token_file {
        crate::write_secret(path, &token)?;
        let added = repo.with_meta(|m| m.add_token(user, &token_hash(&token), &how.label, now()));
        if let Err(e) = added {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        println!(
            "token {} of user {user} is in {}",
            token_id(&token_hash(&token)),
            path.display()
        );
    } else {
        repo.with_meta(|m| m.add_token(user, &token_hash(&token), &how.label, now()))?;
        println!(
            "token {} of user {user}, shown only now:",
            token_id(&token_hash(&token))
        );
        println!("{token}");
    }
    Ok(())
}

fn open(data: &Path) -> Result<Repo> {
    Repo::open_for_admin(data).with_context(|| format!("open {}", data.display()))
}

pub fn user(what: UserCommand) -> Result<()> {
    match what {
        UserCommand::Add { name, token, data } => {
            check_name("user", &name)?;
            let repo = open(&data)?;
            repo.with_meta(|m| m.add_user(&name, now()))?;
            println!("user {name} added, with access to nothing yet");
            issue(&repo, &name, &token)?;
        }
        UserCommand::List { data } => {
            let repo = open(&data)?;
            for u in repo.with_meta(|m| m.users())? {
                let folders: Vec<String> = u
                    .folders
                    .iter()
                    .map(|f| format!("{} ({})", f.folder, f.access))
                    .collect();
                println!(
                    "{}  since {}, {} token(s), folders: {}",
                    u.name,
                    date(u.created),
                    u.tokens,
                    if folders.is_empty() {
                        "none".to_string()
                    } else {
                        folders.join(", ")
                    }
                );
            }
        }
        UserCommand::Remove { name, data } => {
            let repo = open(&data)?;
            repo.with_meta(|m| m.remove_user(&name))?;
            println!("user {name} removed, with its tokens and access");
        }
    }
    Ok(())
}

pub fn token(what: TokenCommand) -> Result<()> {
    match what {
        TokenCommand::Add { user, token, data } => {
            let repo = open(&data)?;
            issue(&repo, &user, &token)?;
        }
        TokenCommand::List { data } => {
            let repo = open(&data)?;
            for t in repo.with_meta(|m| m.tokens())? {
                let label = if t.label.is_empty() {
                    String::new()
                } else {
                    format!("  {}", t.label)
                };
                println!("{}  {}  since {}{label}", t.id, t.user, date(t.created));
            }
        }
        TokenCommand::Remove { id, data } => {
            let id = id.to_ascii_lowercase();
            if id.len() < 4 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!(
                    "{id:?} is no token ID: give at least 4 of the hex digits `token list` shows"
                );
            }
            let repo = open(&data)?;
            let tokens = repo.with_meta(|m| m.tokens())?;
            let found: Vec<_> = tokens
                .iter()
                .filter(|t| t.hash.to_string().starts_with(&id))
                .collect();
            match found.as_slice() {
                [] => bail!("no token {id}"),
                [t] => {
                    repo.with_meta(|m| m.remove_token(&t.hash))?;
                    println!("token {} of user {} removed", t.id, t.user);
                }
                more => bail!("{} tokens start {id}: give more digits", more.len()),
            }
        }
    }
    Ok(())
}

pub fn folder(what: FolderCommand) -> Result<()> {
    match what {
        FolderCommand::Add {
            name,
            managers,
            data,
        } => {
            check_name("folder", &name)?;
            let repo = open(&data)?;
            repo.with_meta(|m| {
                if m.namespace_exists(&name)? {
                    bail!("there is a folder {name:?} already");
                }
                let users = m.users()?;
                for u in &managers {
                    if !users.iter().any(|x| &x.name == u) {
                        bail!("no user {u:?}");
                    }
                }
                m.create_namespace(&name)?;
                for u in &managers {
                    m.set_access(&name, u, Some(Access::Manage))?;
                }
                Ok(())
            })?;
            match managers.as_slice() {
                [] => println!("folder {name} added; nobody has access to it yet"),
                m => println!("folder {name} added, managed by {}", m.join(", ")),
            }
        }
        FolderCommand::List { data } => {
            let repo = open(&data)?;
            for ns in repo.namespaces()? {
                let head = repo.head(&ns)?;
                let members: Vec<String> = repo
                    .with_meta(|m| m.members(&ns))?
                    .iter()
                    .map(|x| format!("{} ({})", x.user, x.access))
                    .collect();
                let former = repo.with_meta(|m| m.former_names(&ns))?;
                println!(
                    "{ns}  at {head}{}, access: {}",
                    if former.is_empty() {
                        String::new()
                    } else {
                        format!(", formerly {}", former.join(", "))
                    },
                    if members.is_empty() {
                        "nobody".to_string()
                    } else {
                        members.join(", ")
                    }
                );
            }
        }
        FolderCommand::Rename { from, to, data } => {
            check_name("folder", &to)?;
            let repo = open(&data)?;
            println!("{}", repo.rename_namespace(&from, &to)?);
        }
    }
    Ok(())
}

pub fn grant(data: &Path, folder: &str, user: &str, access: Option<Access>) -> Result<()> {
    let repo = open(data)?;
    repo.with_meta(|m| {
        if !m.namespace_exists(folder)? {
            bail!("no folder {folder:?}");
        }
        if access.is_none() && m.access(folder, user)?.is_none() {
            bail!("user {user:?} has no access to folder {folder:?}");
        }
        m.set_access(folder, user, access)
    })?;
    match access {
        Some(a) => println!(
            "user {user} may {} folder {folder}",
            match a {
                Access::Read => "read",
                Access::Write => "read and write",
                Access::Manage => "read, write and manage",
            }
        ),
        None => println!("user {user} has no access to folder {folder} any more"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_are_utc_days() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_794_268_800), "2026-11-10");
        assert_eq!(super::date(951_782_400), "2000-02-29");
    }
}
