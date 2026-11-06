//! What this device does not sync: rules in `.cairn/ignore` by path, in `.gitignore` syntax, and server
//! folders excluded by identity (`cairn exclude`), which stay excluded however they are renamed. There
//! is no built-in list of names: with neither, everything syncs.
//!
//! The merge never learns about any of it. It works on a view: the base and the server's tree with
//! the excluded entries taken out, as if they did not exist, while the real trees stay the record of
//! the server. So an excluded entry cannot read as deleted on either side, and nothing about it is
//! ever pushed. The merge's base also loses every entry this device does not hold (`Row::held`), since
//! a delete needs proof that the thing was here. A device that has never had a file gets it, one that
//! removed it because the server said so gets it back if the server changes its mind, and a file
//! changed while it was excluded merges as new, keeping both versions if they differ.

use crate::client::Client;
use crate::fsops::{birth_mark, is_not_found, Dir, FileType, Stat};
use crate::root::Root;
use crate::state::Row;
use anyhow::{anyhow, bail, Context, Result};
use cairn_protocol::{Link, Name, ObjectId, ObjectKind, Timestamp, Tree};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;

/// In the state directory: this device's own rules, not synced, so that devices can differ.
pub const IGNORE_FILE: &[u8] = b"ignore";
const KEY_EXCLUDED: &str = "excluded";
const RULE_READ_ATTEMPTS: u32 = 5;

/// Rules in `.gitignore` syntax, relative to the root, and the hash of the text they were read from.
#[derive(Clone, Default)]
pub struct Rules(Option<Gitignore>, Option<cairn_protocol::Hash>);

impl Rules {
    pub fn none() -> Self {
        Self(None, None)
    }

    /// What the rules were read from, so that a sync can tell they have not changed since another.
    pub fn text_hash(&self) -> Option<cairn_protocol::Hash> {
        self.1
    }

    /// Anything wrong with the rules is an error, never "no rules": those are what keep a secret or a
    /// huge folder from being uploaded.
    pub fn parse(text: &[u8]) -> Result<Self> {
        let hash = cairn_protocol::Hash::of(text);
        let text = std::str::from_utf8(text).map_err(|_| anyhow!("it is not valid UTF-8"))?;
        // Some editors start a file with this, and it would quietly disable the first rule.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut builder = GitignoreBuilder::new("");
        for (i, line) in text.lines().enumerate() {
            // Git knows these, and the matcher takes them without an error and matches nothing.
            if line.contains("[:") {
                bail!(
                    "line {}: classes such as [:digit:] are not supported here: write [0-9]",
                    i + 1
                );
            }
            builder
                .add_line(None, line)
                .with_context(|| format!("line {}", i + 1))?;
        }
        let matcher = builder.build()?;
        Ok(Self(
            (matcher.num_ignores() + matcher.num_whitelists() > 0).then_some(matcher),
            Some(hash),
        ))
    }

    pub fn load(state_dir: &Dir) -> Result<Self> {
        let what = "cannot use .cairn/ignore, and nothing was synced";
        // Saved by truncating and writing, it reads as empty for a moment: the same wait as any file.
        for _ in 0..RULE_READ_ATTEMPTS {
            let st = match state_dir.stat(IGNORE_FILE) {
                Ok(st) => st,
                Err(e) if is_not_found(&e) => return Ok(Self::none()),
                Err(e) => return Err(e).context(what),
            };
            if st.file_type != FileType::RegularFile {
                return Err(anyhow!("it is not a regular file")).context(what);
            }
            crate::scan::wait_until_stable(&st);
            let fd = state_dir.open_read(IGNORE_FILE).context(what)?;
            let mut text = Vec::new();
            std::fs::File::from(fd)
                .read_to_end(&mut text)
                .context(what)?;
            if state_dir.stat(IGNORE_FILE).context(what)?.ctime == st.ctime {
                return Self::parse(&text).context(what);
            }
        }
        Err(anyhow!("it keeps changing")).context(what)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Whether one entry is excluded, given that its parent is not: as in git, nothing under an
    /// excluded directory can be brought back, so a walk stops there and never asks.
    pub fn excludes(&self, path: &[Name], is_dir: bool) -> bool {
        let Some(matcher) = &self.0 else {
            return false;
        };
        let mut rel = PathBuf::new();
        for name in path {
            crate::fsops::push_name(&mut rel, name.as_bytes());
        }
        matcher.matched(&rel, is_dir).is_ignore()
    }
}

/// What `cairn exclude` left here, or may yet stand here: what is at its path, for anything this device
/// has no record of, and the copy itself by its inode wherever it is moved to, here or by a rename on
/// the server. A folder deleted and generated again stays out too, even if the server comes to have
/// something at that path: sync then says what is in the way. Something this device holds is not this.
/// A filesystem without birth times cannot tell a reused inode number from the copy, and a new
/// file that reuses it stays out, listed by `cairn status`, rather than a copy going up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptCopy {
    pub dev: u64,
    pub ino: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btime: Option<Timestamp>,
    /// Where it was last seen.
    pub path: Vec<Name>,
    /// The object this device held here, when a sync kept the copy out by moving it from under its
    /// rule: until the sync that agrees on that drops its record, the record is the copy's own, and
    /// does not make the copy something this device syncs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<ObjectId>,
}

impl KeptCopy {
    fn is(&self, st: &Stat) -> bool {
        // Zero: nothing was here when it was excluded, so only the path says anything.
        self.ino != 0
            && (self.dev, self.ino, birth_mark(self.btime))
                == (st.dev, st.ino, birth_mark(st.btime))
    }

    fn follow(&mut self, path: &[Name], st: &Stat) {
        (self.dev, self.ino, self.btime) = (st.dev, st.ino, birth_mark(st.btime));
        self.path = path.to_vec();
    }
}

/// A scan's sighting of something left out by `cairn exclude`.
pub struct SeenCopy {
    copy: usize,
    by_inode: bool,
    path: Vec<Name>,
    stat: Stat,
}

/// A name on the server that this filesystem cannot hold: its bytes are refused, or another name here
/// is the same name to it. Out of view, as if excluded, so that no sync here fails on it again, until
/// the server's entry moves or, for a twin, the twin here has gone; then it comes down as anything this
/// device never held does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unheld {
    pub object: ObjectId,
    pub link: Link,
    pub why: String,
    /// Another name here stands in its way, and may go.
    pub twin: bool,
}

/// Everything this device does not sync: what it chose to leave out, and what it cannot hold.
#[derive(Clone, Default)]
pub struct Selection {
    pub rules: Rules,
    /// Excluded by identity, wherever on the server they go.
    pub objects: BTreeSet<ObjectId>,
    pub copies: Vec<KeptCopy>,
    pub unheld: Vec<Unheld>,
}

#[derive(Default, Serialize, Deserialize)]
struct Excluded {
    objects: BTreeSet<ObjectId>,
    copies: Vec<KeptCopy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unheld: Vec<Unheld>,
}

/// The trees as this device takes part in them, and what the merge needs to know about the rest.
pub struct View<'a> {
    pub base: Cow<'a, Tree>,
    pub remote: Cow<'a, Tree>,
    pub hidden: crate::merge::Hidden,
    /// Records of objects now excluded on both sides, which must go: lifting the exclusion later has
    /// to find this device with no claim on them, however their copies here have changed meanwhile.
    pub purge: Vec<ObjectId>,
    /// Records that gave up entries now out of view.
    pub changed: Vec<ObjectId>,
}

struct Visible {
    tree: Tree,
    /// The topmost entries left out, each with its object.
    left_out: Vec<(Link, ObjectId)>,
}

/// The part of a tree that passes the rules and `hide`, walked from the root down.
fn visible(tree: &Tree, rules: &Rules, hide: &dyn Fn(ObjectId, &Link) -> bool) -> Visible {
    let mut kept: BTreeMap<ObjectId, BTreeSet<Link>> = BTreeMap::new();
    let mut left_out = Vec::new();
    let mut stack = vec![(ObjectId::ROOT, Vec::new())];
    while let Some((dir, path)) = stack.pop() {
        for (name, child) in tree.children(dir) {
            let Some(node) = tree.nodes.get(&child) else {
                continue;
            };
            let is_dir = node.kind() == ObjectKind::Dir;
            let mut at: Vec<Name> = path.clone();
            at.push(name.clone());
            let link = Link {
                parent: dir,
                name: name.clone(),
            };
            if hide(child, &link) || rules.excludes(&at, is_dir) {
                left_out.push((link, child));
            } else {
                kept.entry(child).or_default().insert(link);
                if is_dir {
                    stack.push((child, at));
                }
            }
        }
    }
    let nodes = tree
        .nodes
        .iter()
        .filter_map(|(id, n)| {
            if *id == ObjectId::ROOT || n.links.is_empty() {
                return Some((*id, n.clone()));
            }
            let links = kept.get(id)?;
            let mut n = n.clone();
            n.links = links.clone();
            Some((*id, n))
        })
        .collect();
    Visible {
        tree: Tree::from_nodes(nodes),
        left_out,
    }
}

/// The entries of an object in the base that this device holds.
fn held_links<'t>(
    id: ObjectId,
    links: &'t BTreeSet<Link>,
    rows: &BTreeMap<ObjectId, Row>,
) -> Cow<'t, BTreeSet<Link>> {
    match rows.get(&id).map(|r| r.held.as_ref()) {
        None => Cow::Owned(BTreeSet::new()),
        Some(None) => Cow::Borrowed(links),
        Some(Some(held)) if links.is_subset(held) => Cow::Borrowed(links),
        Some(Some(held)) => Cow::Owned(links.intersection(held).cloned().collect()),
    }
}

/// The base without what this device does not hold. `None` if that is all of it, as it nearly always is.
fn held_only(base: &Tree, rows: &BTreeMap<ObjectId, Row>) -> Option<Tree> {
    let whole = |id: &ObjectId, links: &BTreeSet<Link>| {
        *id == ObjectId::ROOT
            || links.is_empty()
            || matches!(held_links(*id, links, rows), Cow::Borrowed(_))
    };
    if base.nodes.iter().all(|(id, n)| whole(id, &n.links)) {
        return None;
    }
    let nodes = base
        .nodes
        .iter()
        .filter_map(|(id, n)| {
            if whole(id, &n.links) {
                return Some((*id, n.clone()));
            }
            let links = held_links(*id, &n.links, rows).into_owned();
            if links.is_empty() {
                return None;
            }
            let mut n = n.clone();
            n.links = links;
            Some((*id, n))
        })
        .collect();
    Some(Tree::from_nodes(nodes))
}

/// Whether this inode is an object this device has a record of. A birth time that differs proves the
/// number was reused.
fn is_known(rows: &BTreeMap<ObjectId, Row>, st: &Stat) -> bool {
    is_known_but(rows, st, None)
}

/// The same, but for the record of one object.
fn is_known_but(rows: &BTreeMap<ObjectId, Row>, st: &Stat, but: Option<ObjectId>) -> bool {
    rows.iter()
        .any(|(id, r)| but != Some(*id) && is_its_record(r, st))
}

fn is_its_record(row: &Row, st: &Stat) -> bool {
    (row.fp.dev, row.fp.ino, birth_mark(row.fp.btime)) == (st.dev, st.ino, birth_mark(st.btime))
}

impl Selection {
    pub fn load(root: &Root) -> Result<Self> {
        let excluded = load_excluded(root)?;
        Ok(Self {
            rules: Rules::load(&root.state_dir)?,
            objects: excluded.objects,
            copies: excluded.copies,
            unheld: excluded.unheld,
        })
    }

    fn is_unheld(&self, id: ObjectId, link: &Link) -> bool {
        self.unheld
            .iter()
            .any(|u| u.object == id && &u.link == link)
    }

    /// Drop what no longer stands in the way: an entry the server has moved or removed, or one whose
    /// twin here has gone, or has been renamed to the server's very name, which then merges with it as
    /// anything new does. Returns whether any went.
    pub fn recheck_unheld(&mut self, root: &Dir, remote: &Tree) -> bool {
        let before = self.unheld.len();
        let mut listed: HashMap<Vec<Name>, Vec<Vec<u8>>> = HashMap::new();
        self.unheld.retain(|u| {
            if !remote
                .get_live(u.object)
                .is_some_and(|n| n.links.contains(&u.link))
            {
                return false;
            }
            if !u.twin {
                return true;
            }
            let Some(path) = remote.path_of_link(&u.link) else {
                return true;
            };
            let folder = &path[..path.len() - 1];
            let Ok(dir) = root.open_path(folder) else {
                return true;
            };
            if matches!(dir.stat(u.link.name.as_bytes()), Err(e) if is_not_found(&e)) {
                return false;
            }
            let names = listed
                .entry(folder.to_vec())
                .or_insert_with(|| dir.list().unwrap_or_default());
            !names.iter().any(|n| n == u.link.name.as_bytes())
        });
        self.unheld.len() != before
    }

    /// A filter for scans: whether this entry is left out, and where the copies went.
    pub fn leaves_out(
        &self,
        path: &[Name],
        st: &Stat,
        view: &View<'_>,
        rows: &BTreeMap<ObjectId, Row>,
        seen: &mut Vec<SeenCopy>,
    ) -> bool {
        if self
            .rules
            .excludes(path, st.file_type == FileType::Directory)
        {
            return true;
        }
        if self.copies.is_empty() {
            return false;
        }
        // What a sync kept out of what this device held is read as gone from here, as it was when the
        // rule still covered it, by its own record too: that goes once the sync agrees.
        let own = self.copies.iter().find(|c| c.is(st)).and_then(|c| c.was);
        let other = |id: &ObjectId| rows.contains_key(id) && own != Some(*id);
        // A name this device holds is not a copy's, whatever is at it now: leaving that out would read
        // what is held as deleted. It has a record if it came from the server in a sync not finished.
        let held = |tree: &Tree| tree.resolve(path).is_some_and(|id| other(&id));
        if held(&view.base) || held(&view.remote) {
            return false;
        }
        // Known, it has become something this device syncs, by a move onto a held name, say, and a
        // record of the copy it once was means nothing now.
        let found = if is_known_but(rows, st, own) {
            None
        } else if let Some(i) = self.copies.iter().position(|c| c.is(st)) {
            Some((i, true))
        } else {
            self.copies
                .iter()
                .position(|c| c.path == path)
                .map(|i| (i, false))
        };
        let Some((copy, by_inode)) = found else {
            return false;
        };
        seen.push(SeenCopy {
            copy,
            by_inode,
            path: path.to_vec(),
            stat: st.clone(),
        });
        true
    }

    /// What `leaves_out` says, without the trees: the same, but for an entry at a copy's name or inode
    /// that this device holds or knows, which `leaves_out` keeps in and this leaves out. For a scan that
    /// is compared whole with one `leaves_out` filtered: where the two differ, the scans do too.
    pub fn leaves_out_roughly(&self, path: &[Name], st: &Stat) -> bool {
        self.rules
            .excludes(path, st.file_type == FileType::Directory)
            || self.copies.iter().any(|c| c.is(st) || c.path == path)
    }

    /// Bring the records up to date with a scan. Returns whether any changed.
    pub fn follow_copies(&mut self, seen: &[SeenCopy]) -> bool {
        let before = self.copies.clone();
        let found_itself: BTreeSet<usize> =
            seen.iter().filter(|s| s.by_inode).map(|s| s.copy).collect();
        for s in seen {
            if s.by_inode || !found_itself.contains(&s.copy) {
                self.copies[s.copy].follow(&s.path, &s.stat);
            } else if !self.copies.iter().any(|c| c.is(&s.stat)) {
                // The copy has moved away and something new stands at its old path: both stay out.
                let mut new = self.copies[s.copy].clone();
                new.follow(&s.path, &s.stat);
                self.copies.push(new);
            }
        }
        self.copies != before
    }

    /// What a rule leaves out in a folder this sync is about to move, where the rule will not match
    /// it: nobody here chose that, so it stays out by inode, as a copy left by `cairn exclude` does.
    /// So does what this device held there, which its record would otherwise bring back into view
    /// where the move takes it, but for a file with another name here, which by inode would take that
    /// name out of view too. Returns the paths, whose records are not durable until `store`.
    pub fn keep_out_what_moves(
        &mut self,
        root: &Root,
        excluded: &[Vec<Name>],
        moving: &BTreeMap<Vec<Name>, Vec<Name>>,
        rows: &BTreeMap<ObjectId, Row>,
    ) -> Vec<Vec<Name>> {
        let mut kept = Vec::new();
        for path in excluded {
            let Some((name, parent)) = path.split_last() else {
                continue;
            };
            let Some(to) = moving.get(parent) else {
                continue;
            };
            let Some(st) = stat_here(root, path) else {
                continue;
            };
            let is_dir = st.file_type == FileType::Directory;
            let mut after = to.clone();
            after.push(name.clone());
            if !self.rules.excludes(path, is_dir)
                || self.rules.excludes(&after, is_dir)
                || self.copies.iter().any(|c| c.is(&st))
            {
                continue;
            }
            let was = rows
                .iter()
                .find(|(_, r)| is_its_record(r, &st))
                .map(|(id, _)| *id);
            if was.is_some() && !is_dir && st.nlink > 1 {
                continue;
            }
            let mut copy = KeptCopy {
                dev: 0,
                ino: 0,
                btime: None,
                path: Vec::new(),
                was,
            };
            copy.follow(path, &st);
            self.copies.push(copy);
            kept.push(path.clone());
        }
        kept
    }

    pub fn store(&self, root: &Root) -> Result<()> {
        store_excluded(
            root,
            &Excluded {
                objects: self.objects.clone(),
                copies: self.copies.clone(),
                unheld: self.unheld.clone(),
            },
        )
    }

    /// Takes the records of what has gone out of view from `rows`. The base is not final until
    /// `settle`, which needs a scan.
    pub fn view<'a>(
        &self,
        base: &'a Tree,
        remote: &'a Tree,
        rows: &mut BTreeMap<ObjectId, Row>,
    ) -> View<'a> {
        if self.rules.is_empty() && self.objects.is_empty() && self.unheld.is_empty() {
            return View {
                base: Cow::Borrowed(base),
                remote: Cow::Borrowed(remote),
                hidden: Default::default(),
                purge: Vec::new(),
                changed: Vec::new(),
            };
        }
        let remote_view = visible(remote, &self.rules, &|id, link| {
            self.objects.contains(&id) || self.is_unheld(id, link)
        });
        // A folder excluded by identity while this device still holds it stays in the base, so that
        // it reads as gone from the server and the copy here goes the way anything deleted there
        // does: only if it has not changed. That is `cairn exclude --remove-local`.
        let base_view = visible(base, &self.rules, &|id, link| {
            (self.objects.contains(&id) && !rows.contains_key(&id)) || self.is_unheld(id, link)
        });

        let moved_away: BTreeSet<ObjectId> = remote
            .live()
            .map(|(id, _)| id)
            .filter(|id| !remote_view.tree.is_live(*id))
            .collect();
        let purge: Vec<ObjectId> = rows
            .keys()
            .filter(|id| remote.is_live(**id) || base.is_live(**id))
            .filter(|id| !remote_view.tree.is_live(**id) && !base_view.tree.is_live(**id))
            .copied()
            .collect();
        for id in &purge {
            rows.remove(id);
        }
        // The same for one name of a file with several.
        let mut changed = Vec::new();
        for (id, row) in rows.iter_mut() {
            let links_in = |tree: &Tree| {
                tree.nodes
                    .get(id)
                    .map(|n| n.links.clone())
                    .unwrap_or_default()
            };
            let real: BTreeSet<Link> = links_in(base).union(&links_in(remote)).cloned().collect();
            let seen: BTreeSet<Link> = links_in(&base_view.tree)
                .union(&links_in(&remote_view.tree))
                .cloned()
                .collect();
            if real.is_subset(&seen) {
                continue;
            }
            let held = row.held.get_or_insert_with(|| links_in(base));
            let before = held.len();
            for link in real.difference(&seen) {
                held.remove(link);
            }
            if held.len() != before {
                changed.push(*id);
            }
        }
        let hidden = crate::merge::Hidden {
            pinned: remote_view
                .left_out
                .iter()
                .map(|(link, _)| link.parent)
                .filter(|p| *p != ObjectId::ROOT && remote_view.tree.is_live(*p))
                .collect(),
            reserved: remote_view
                .left_out
                .iter()
                .map(|(l, _)| l.clone())
                .collect(),
            moved_away,
        };
        View {
            base: Cow::Owned(base_view.tree),
            remote: Cow::Owned(remote_view.tree),
            hidden,
            purge,
            changed,
        }
    }
}

impl View<'_> {
    /// The base as the merge may see it: only what this device holds.
    pub fn settle(&mut self, rows: &BTreeMap<ObjectId, Row>) {
        if let Some(held) = held_only(&self.base, rows) {
            self.base = Cow::Owned(held);
        }
    }
}

fn load_excluded(root: &Root) -> Result<Excluded> {
    Ok(match root.state.get(KEY_EXCLUDED)? {
        Some(text) => serde_json::from_str(&text).context("read the excluded folders")?,
        None => Excluded::default(),
    })
}

fn store_excluded(root: &Root, excluded: &Excluded) -> Result<()> {
    root.state
        .set(KEY_EXCLUDED, &serde_json::to_string(excluded)?)
}

/// The server's tree now.
fn remote_tree(root: &Root) -> Result<Tree> {
    let client = Client::new(&root.config)?;
    let mut tree = root.state.load_base()?;
    let mut seq = root.state.cursor()?;
    crate::sync::fetch_changes(&client, &mut tree, &mut seq, &mut Default::default())?;
    Ok(tree)
}

/// Refuse early what cannot be excluded, before a sync uploads anything for it.
pub fn check_excludable(root: &Root, path: &[Name]) -> Result<()> {
    let tree = remote_tree(root)?;
    if tree
        .resolve(path)
        .filter(|id| *id != ObjectId::ROOT)
        .is_none()
    {
        bail!("that is not on the server, so there is nothing to exclude, and nothing was synced; a rule in .cairn/ignore keeps a local path from being uploaded");
    }
    Ok(())
}

/// What is at a path here, without following anything.
fn stat_here(root: &Root, path: &[Name]) -> Option<Stat> {
    let (name, parents) = path.split_last()?;
    let dir = root.dir.open_path(parents.iter()).ok()?;
    dir.try_stat(name.as_bytes()).ok()?
}

/// Whether anything in this folder, at any depth, is one of these inodes.
fn holds_any(dir: &Dir, inodes: &HashSet<(u64, u64, Option<Timestamp>)>) -> Result<bool> {
    for name in dir.list()? {
        let Some(st) = dir.try_stat(&name)? else {
            continue;
        };
        if inodes.contains(&(st.dev, st.ino, birth_mark(st.btime)))
            || (st.file_type == FileType::Directory && holds_any(&dir.open_dir(&name)?, inodes)?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Every inode in this folder, at any depth.
fn inodes_in(dir: &Dir, out: &mut HashSet<(u64, u64, Option<Timestamp>)>) -> Result<()> {
    for name in dir.list()? {
        let Some(st) = dir.try_stat(&name)? else {
            continue;
        };
        out.insert((st.dev, st.ino, birth_mark(st.btime)));
        if st.file_type == FileType::Directory {
            inodes_in(&dir.open_dir(&name)?, out)?;
        }
    }
    Ok(())
}

/// Stop syncing a path on the server. Unless `remove_local`, the copy here stops being looked at and
/// stays until someone deletes it, which then means nothing to the server. Either way, what this device
/// has no record of stays out at that path from now on.
pub fn exclude(root: &mut Root, path: &[Name], remove_local: bool) -> Result<()> {
    let tree = remote_tree(root)?;
    let Some(id) = tree.resolve(path).filter(|id| *id != ObjectId::ROOT) else {
        bail!("that is not on the server, so there is nothing to exclude; a rule in .cairn/ignore keeps a local path from being uploaded");
    };
    let mut excluded = load_excluded(root)?;
    if excluded.objects.contains(&id) {
        bail!("that is already excluded; a copy left here is yours to delete, and that means nothing to the server");
    }
    // What is at that path has to be what the server has there, or the wrong thing would be left out
    // and the right one read as deleted or as new. So does what is under it, as far as this device
    // holds any. With no record of any of it there is nothing to confuse, however far behind it is.
    let rows = root.state.load_rows()?;
    let base = root.state.load_base()?;
    let here = stat_here(root, path);
    let same = match (rows.get(&id), &here) {
        (Some(row), Some(st)) => (row.fp.dev, row.fp.ino) == (st.dev, st.ino),
        (Some(_), None) => false,
        (None, Some(st)) => !is_known(&rows, st),
        (None, None) => true,
    };
    let subtree = tree.subtree(id);
    let under_is_in_step = (!base.is_live(id) && !subtree.iter().any(|o| rows.contains_key(o)))
        || (base.subtree(id) == subtree
            && subtree
                .iter()
                .all(|o| base.nodes.get(o) == tree.nodes.get(o)));
    let moved_in = match &here {
        Some(st) if st.file_type == FileType::Directory => {
            let (name, parents) = path.split_last().expect("not the root");
            let dir = root
                .dir
                .open_path(parents.iter())?
                .open_dir(name.as_bytes())?;
            let outside: HashSet<_> = rows
                .iter()
                .filter(|(id, _)| !subtree.contains(id))
                .map(|(_, r)| (r.fp.dev, r.fp.ino, birth_mark(r.fp.btime)))
                .collect();
            holds_any(&dir, &outside)?
        }
        _ => false,
    };
    if !same || !under_is_in_step || moved_in {
        bail!("this device and the server do not agree yet about what is at that path, and nothing was changed. `cairn sync` would settle that, and send the server whatever was deleted or moved here. If that is not wanted, put it back first, or leave the path out with a rule in .cairn/ignore, which needs no agreement");
    }
    let mut copy = KeptCopy {
        dev: 0,
        ino: 0,
        btime: None,
        path: path.to_vec(),
        was: None,
    };
    if !remove_local {
        // No claim on them from now on, or a delete of the copy here would read as a real one when
        // they are included again. In this order: dying in between leaves files that merge as new.
        let gone: Vec<ObjectId> = tree.subtree(id).into_iter().collect();
        root.state.store_rows(&[], &gone)?;
        if let Some(st) = stat_here(root, path) {
            copy.follow(path, &st);
        }
    }
    if !excluded.copies.contains(&copy) {
        excluded.copies.push(copy);
    }
    excluded.objects.insert(id);
    store_excluded(root, &excluded)?;
    cairn_failpoint::hit("selection.excluded");
    Ok(())
}

/// Sync something excluded again: a folder on the server, the path of a copy left here, or both when
/// they are the same. A copy merges as new, so an old one can bring back what was deleted since and
/// leave conflict copies for everyone: that has to be asked for.
pub fn include(root: &mut Root, path: &[Name], merge: bool) -> Result<()> {
    let tree = remote_tree(root)?;
    let mut excluded = load_excluded(root)?;
    let rows = root.state.load_rows()?;
    let object = tree
        .resolve(path)
        .filter(|id| excluded.objects.contains(id));
    let here = stat_here(root, path);
    let copy = excluded
        .copies
        .iter()
        .position(|c| c.path == path || here.as_ref().is_some_and(|st| c.is(st)));
    if object.is_none() && copy.is_none() {
        bail!("that was not excluded with `cairn exclude`, here or on the server; a rule in .cairn/ignore comes out by editing that file");
    }
    if let Some(i) = copy {
        if here
            .as_ref()
            .is_some_and(|st| excluded.copies[i].is(st) || !is_known(&rows, st))
            && !merge
        {
            bail!("a copy from when this was excluded is at that path, and nothing was changed. Merging it uploads whatever differs from the server, including what has been deleted there since, and keeps both versions of what changed: to do that, pass --merge. Otherwise delete or move it first");
        }
        // A copy a sync kept out of what this device held still has the records of what is in it until a
        // sync agrees, and one the server's is in the way of never does: they go first, or it would merge
        // as what was held, and what the server has deleted since would go from here, edited or not.
        // Dying in between leaves the copy out, with nothing to vouch for it.
        if let Some(st) = here
            .as_ref()
            .filter(|st| excluded.copies[i].was.is_some() && excluded.copies[i].is(st))
        {
            let mut inodes = HashSet::from([(st.dev, st.ino, birth_mark(st.btime))]);
            if st.file_type == FileType::Directory {
                let (name, parents) = path.split_last().expect("not the root");
                let dir = root
                    .dir
                    .open_path(parents.iter())?
                    .open_dir(name.as_bytes())?;
                inodes_in(&dir, &mut inodes)?;
            }
            let gone: Vec<ObjectId> = rows
                .iter()
                .filter(|(_, r)| inodes.contains(&(r.fp.dev, r.fp.ino, birth_mark(r.fp.btime))))
                .map(|(id, _)| *id)
                .collect();
            root.state.store_rows(&[], &gone)?;
        }
        // With nothing at that path, a copy may only have moved away: it stays out, wherever it is.
        if here.is_none() && excluded.copies[i].ino != 0 {
            excluded.copies[i].path = Vec::new();
        } else {
            excluded.copies.remove(i);
        }
    }
    if let Some(id) = object {
        excluded.objects.remove(&id);
    }
    store_excluded(root, &excluded)
}

#[derive(Default)]
pub struct ExcludedPaths {
    pub on_server: Vec<Vec<Name>>,
    /// Each path left out here, and whether anything is at it now.
    pub copies: Vec<(Vec<Name>, bool)>,
}

/// The paths on the server that are excluded by identity as they are now, and the copies left here.
pub fn excluded_paths(root: &Root) -> Result<ExcludedPaths> {
    let tree = remote_tree(root)?;
    let excluded = load_excluded(root)?;
    let rows = root.state.load_rows()?;
    let on_server = excluded
        .objects
        .into_iter()
        .filter_map(|id| {
            let link = tree.get_live(id)?.links.iter().next()?;
            tree.path_of_link(link)
        })
        .collect();
    Ok(ExcludedPaths {
        on_server,
        copies: excluded
            .copies
            .into_iter()
            .filter(|c| !c.path.is_empty())
            .map(|c| {
                let there =
                    stat_here(root, &c.path).is_some_and(|st| c.is(&st) || !is_known(&rows, &st));
                (c.path, there)
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{Content, Metadata, Node, RevisionBody, RevisionId};

    /// Where names are bytes nothing is ever a twin, so the twin is made up: the server's `Readme`
    /// held out for a `README` here, which a person then renames to the server's very name.
    #[test]
    fn a_twin_renamed_to_the_servers_name_comes_back_into_view() {
        let path = std::env::temp_dir().join(format!("cairn-twin-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&path).unwrap();
        let mut remote = Tree::new();
        let object = ObjectId::random();
        let link = Link {
            parent: ObjectId::ROOT,
            name: Name::new(*b"Readme").unwrap(),
        };
        remote
            .apply(&cairn_protocol::Op::Create {
                object,
                revision: RevisionId::random(),
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: Metadata::default(),
                },
                link: link.clone(),
                conflict_of: None,
            })
            .unwrap();
        let mut selection = Selection {
            unheld: vec![Unheld {
                object,
                link,
                why: "`README` here is the same name to this filesystem".into(),
                twin: true,
            }],
            ..Selection::default()
        };
        let root = Dir::open_root(&path).unwrap();
        std::fs::create_dir(path.join("README")).unwrap();
        std::fs::rename(path.join("README"), path.join("Readme")).unwrap();
        assert!(selection.recheck_unheld(&root, &remote));
        assert!(selection.unheld.is_empty());
        std::fs::remove_dir_all(&path).unwrap();
    }

    fn names(path: &str) -> Vec<Name> {
        path.split('/')
            .map(|s| Name::new(s.as_bytes().to_vec()).unwrap())
            .collect()
    }

    /// A walk stops at an excluded directory, as a scan does.
    fn excluded_on_the_way_down(rules: &Rules, path: &str, is_dir: bool) -> bool {
        let parts = names(path);
        (1..=parts.len()).any(|n| rules.excludes(&parts[..n], n < parts.len() || is_dir))
    }

    #[test]
    fn no_rules_exclude_nothing_at_all() {
        let rules = Rules::parse(b"").unwrap();
        assert!(rules.is_empty());
        for path in [
            ".git/config",
            "node_modules/x",
            ".venv/bin/python",
            ".DS_Store",
            "a~",
        ] {
            assert!(!excluded_on_the_way_down(&rules, path, false), "{path}");
        }
        let comments = Rules::parse(b"# nothing here\n\n").unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn bad_rules_are_an_error_and_never_no_rules() {
        assert!(Rules::parse(b"secret\n[z-a\n").is_err());
        assert!(Rules::parse(b"\xff\xfe\n").is_err());
    }

    #[test]
    fn names_that_are_not_utf8_are_matched_as_bytes() {
        let rules = Rules::parse(b"*.log\n").unwrap();
        let odd = Name::new(b"caf\xe9.log".to_vec()).unwrap();
        assert!(rules.excludes(&[odd], false));
        let keep = Name::new(b"caf\xe9.txt".to_vec()).unwrap();
        assert!(!rules.excludes(&[keep], false));
    }

    /// Git itself is the reference for what the syntax means.
    #[test]
    fn rules_mean_what_they_mean_to_git() {
        let git = |dir: &std::path::Path, args: &[&str], input: &str| -> Option<String> {
            use std::io::Write;
            let mut child = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let dir = std::env::temp_dir().join(format!("cairn-rules-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir, &["init", "-q"], "").is_none() {
            eprintln!("no git here: skipped");
            return;
        }
        let rule_sets: &[&str] = &[
            "*.log",
            "build/",
            "/build",
            "doc/*.txt",
            "doc/**/*.txt",
            "**/cache",
            "a/**",
            "*.log\n!keep.log",
            "build/*\n!build/keep",
            "build/\n!build/keep",
            "/*\n!/src",
            "/*\n!/src/\n/src/*\n!/src/a",
            "x?z",
            "[ab]c",
            "[!a]c",
            "\\#hash",
            "\\!bang",
            "trailing   ",
            "foo/bar",
            "foo/bar/",
            "**/bar/*.log",
            "foo/**/baz",
            "*",
            "*\n!*/\n!*.txt",
        ];
        let paths = [
            "a.log",
            "keep.log",
            "x/a.log",
            "x/keep.log",
            "build/keep",
            "build/other",
            "x/build/keep",
            "doc/a.txt",
            "doc/sub/a.txt",
            "x/doc/a.txt",
            "cache/f",
            "x/cache/f",
            "x/y/cache",
            "a/f",
            "a/b/f",
            "src/a",
            "src/b",
            "other",
            "xyz",
            "xz",
            "ac",
            "bc",
            "cc",
            "#hash",
            "!bang",
            "trailing",
            "foo/bar",
            "foo/bar/x",
            "x/foo/bar",
            "x/bar/f.log",
            "foo/x/y/baz",
            "foo/baz",
            "f.txt",
            "x/f.txt",
        ];
        // Deepest first, so that a path that is also a parent ends up a directory.
        let mut deepest = paths;
        deepest.sort_by_key(|p| std::cmp::Reverse(p.len()));
        for path in deepest {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            if !full.exists() {
                std::fs::write(&full, b"").unwrap();
            }
        }
        for set in rule_sets {
            std::fs::write(dir.join(".gitignore"), set).unwrap();
            let rules = Rules::parse(set.as_bytes()).unwrap();
            for is_dir in [false, true] {
                for path in paths {
                    // Only ask about what the tree really holds at that kind.
                    if dir.join(path).is_dir() != is_dir {
                        continue;
                    }
                    let answer = git(
                        &dir,
                        &["check-ignore", "--no-index", "--stdin"],
                        &format!("{path}\n"),
                    )
                    .unwrap();
                    assert_eq!(
                        excluded_on_the_way_down(&rules, path, is_dir),
                        !answer.trim().is_empty(),
                        "rules {set:?} on {path}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn at(parent: ObjectId, name: &str) -> Link {
        Link {
            parent,
            name: Name::new(name.as_bytes().to_vec()).unwrap(),
        }
    }

    fn node(content: Content, link: Link) -> Node {
        Node {
            rev: RevisionId::random(),
            body: RevisionBody {
                content,
                metadata: Metadata::default(),
            },
            links: [link].into(),
        }
    }

    fn file() -> Content {
        Content::File {
            manifest: cairn_protocol::Hash::of(b"x"),
            size: 1,
        }
    }

    fn row() -> Row {
        let stamp = cairn_protocol::Timestamp { sec: 1, nsec: 0 };
        Row {
            fp: crate::state::Fingerprint {
                dev: 1,
                ino: 1,
                size: 1,
                mtime: stamp,
                ctime: stamp,
                btime: None,
            },
            racy: false,
            content: file(),
            scan: Metadata::default(),
            applied: None,
            synced: None,
            incomplete: false,
            held: None,
        }
    }

    fn selection(rules: &str, objects: &[ObjectId]) -> Selection {
        Selection {
            rules: Rules::parse(rules.as_bytes()).unwrap(),
            objects: objects.iter().copied().collect(),
            copies: Vec::new(),
            unheld: Vec::new(),
        }
    }

    /// The view as a sync reaches it, with nothing found on disk that changes who holds what.
    fn settled<'a>(
        s: &Selection,
        base: &'a Tree,
        remote: &'a Tree,
        rows: &mut BTreeMap<ObjectId, Row>,
    ) -> View<'a> {
        let mut view = s.view(base, remote, rows);
        view.settle(rows);
        view
    }

    /// `d/` holding `d/kept` and `d/x.log`, with `x.log` hidden by rule.
    fn tree_with_a_log() -> (Tree, ObjectId, ObjectId, ObjectId) {
        let (d, kept, log) = (ObjectId::random(), ObjectId::random(), ObjectId::random());
        let nodes = [
            (d, node(Content::Dir, at(ObjectId::ROOT, "d"))),
            (kept, node(file(), at(d, "kept"))),
            (log, node(file(), at(d, "x.log"))),
        ]
        .into_iter()
        .collect();
        (Tree::from_nodes(nodes), d, kept, log)
    }

    #[test]
    fn a_rule_takes_an_entry_out_of_both_trees_and_records_why_it_matters() {
        let (tree, d, kept, log) = tree_with_a_log();
        let mut rows = BTreeMap::from([(d, row()), (kept, row()), (log, row())]);
        let view = settled(&selection("*.log", &[]), &tree, &tree, &mut rows);
        assert!(view.base.is_live(kept) && view.remote.is_live(kept));
        assert!(!view.base.is_live(log) && !view.remote.is_live(log));
        // The real tree still says the folder is not empty and the name is taken.
        assert_eq!(view.hidden.pinned, BTreeSet::from([d]));
        assert_eq!(view.hidden.reserved, BTreeSet::from([at(d, "x.log")]));
        assert_eq!(view.hidden.moved_away, BTreeSet::from([log]));
        assert_eq!(view.purge, vec![log]);
        assert!(!rows.contains_key(&log));
    }

    /// The whole point of the base losing unheld objects: without it this reads as a delete.
    #[test]
    fn what_this_device_never_held_is_not_in_the_base_even_with_no_rules() {
        let (tree, d, kept, log) = tree_with_a_log();
        let mut rows = BTreeMap::from([(d, row()), (kept, row())]);
        let view = settled(&selection("", &[]), &tree, &tree, &mut rows);
        assert!(view.base.is_live(kept));
        assert!(!view.base.is_live(log));
        assert!(view.remote.is_live(log));
    }

    #[test]
    fn a_folder_excluded_by_identity_stays_excluded_when_renamed() {
        let (tree, d, kept, _) = tree_with_a_log();
        let mut renamed = tree.clone();
        renamed
            .apply(&cairn_protocol::Op::Move {
                object: d,
                from: at(ObjectId::ROOT, "d"),
                to: at(ObjectId::ROOT, "renamed"),
            })
            .unwrap();
        let view = settled(&selection("", &[d]), &tree, &renamed, &mut BTreeMap::new());
        assert!(!view.remote.is_live(d) && !view.remote.is_live(kept));
        assert!(!view.base.is_live(d));
    }

    #[test]
    fn a_folder_excluded_while_still_held_stays_in_the_base_until_it_has_gone() {
        let (tree, d, kept, log) = tree_with_a_log();
        let mut rows = BTreeMap::from([(d, row()), (kept, row()), (log, row())]);
        let view = settled(&selection("", &[d]), &tree, &tree, &mut rows);
        assert!(view.base.is_live(d) && view.base.is_live(kept));
        assert!(!view.remote.is_live(d) && !view.remote.is_live(kept));
        assert!(view.purge.is_empty());
    }

    #[test]
    fn a_hard_link_under_an_excluded_path_hides_only_that_name() {
        let (d, f) = (ObjectId::random(), ObjectId::random());
        let mut n = node(file(), at(ObjectId::ROOT, "seen"));
        n.links.insert(at(d, "unseen"));
        let nodes = [(d, node(Content::Dir, at(ObjectId::ROOT, "d"))), (f, n)]
            .into_iter()
            .collect();
        let tree = Tree::from_nodes(nodes);
        let mut rows = BTreeMap::from([(f, row())]);
        let view = settled(&selection("d/", &[]), &tree, &tree, &mut rows);
        assert_eq!(
            view.remote.nodes[&f].links,
            BTreeSet::from([at(ObjectId::ROOT, "seen")])
        );
        assert!(!view.hidden.moved_away.contains(&f));
        assert!(view.purge.is_empty());
        // Given up for good: when the rule goes, this name reads as new, not as removed here.
        assert_eq!(view.changed, vec![f]);
        assert_eq!(
            rows[&f].held,
            Some(BTreeSet::from([at(ObjectId::ROOT, "seen")]))
        );
        let lifted = settled(&selection("", &[]), &tree, &tree, &mut rows);
        assert_eq!(
            lifted.base.nodes[&f].links,
            BTreeSet::from([at(ObjectId::ROOT, "seen")])
        );
        assert_eq!(lifted.remote.nodes[&f].links.len(), 2);
    }

    #[test]
    fn a_rule_file_starting_with_a_byte_order_mark_still_means_its_first_rule() {
        let rules = Rules::parse("\u{feff}secret.key\n".as_bytes()).unwrap();
        assert!(rules.excludes(&names("secret.key"), false));
    }

    /// The same number on a different birth time is a reused inode: nothing this device knows. Not on
    /// Windows, where a file ID is never reused.
    #[cfg(unix)]
    #[test]
    fn a_reused_inode_number_is_not_known() {
        let mut st = Dir::open_root(std::path::Path::new("/"))
            .unwrap()
            .stat_self()
            .unwrap();
        let mut r = row();
        r.fp.dev = st.dev;
        r.fp.ino = st.ino;
        r.fp.btime = Some(Timestamp { sec: 100, nsec: 0 });
        let rows = BTreeMap::from([(ObjectId::random(), r)]);
        st.btime = Some(Timestamp { sec: 100, nsec: 0 });
        assert!(is_known(&rows, &st));
        st.btime = Some(Timestamp { sec: 200, nsec: 0 });
        assert!(!is_known(&rows, &st));
    }

    #[test]
    fn a_class_the_matcher_would_silently_not_match_is_refused() {
        assert!(Rules::parse(b"[[:digit:]]*.key\n").is_err());
        assert!(Rules::parse(b"[0-9]*.key\n").is_ok());
    }
}
