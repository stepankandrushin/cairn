//! A new name here that the server holds in another form: one that differs from the server's name in
//! the same folder only in a way this filesystem does not tell apart, Unicode normalization or, where
//! it folds case, case. APFS takes a decomposed `é` for the composed one, so a Mac whose tree came from
//! a tool that wrote names decomposed would send each such name up as a second name, a folder with all
//! that is in it, and every device that keeps names as bytes would then hold both. Until filename
//! policies translate names (PLAN.md, Filenames), such an entry waits, reported with both names, until it
//! is renamed here to the server's form; then the two merge like any copies of one thing.

use crate::identify::Identified;
use crate::merge::LNode;
use crate::root::Root;
use cairn_protocol::{Link, Name, ObjectId, ObjectKind, Skip, SkipKind, Tree};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Twin {
    /// The new object here, and its entry.
    pub object: ObjectId,
    pub here: Link,
    /// The server's name in that folder, which this filesystem takes for the same name.
    pub server: Name,
}

/// What is in one local folder, by name.
pub type Folder<'a> = BTreeMap<&'a Name, ObjectId>;

/// The new objects here that stand at a name the server has in another form, in the same folder, and
/// not in this form too. `same_here(folder, name, in_it)` says which of what is in that local folder, if
/// anything, this filesystem finds under the server's `name`.
pub fn find(
    local: &BTreeMap<ObjectId, LNode>,
    known: &dyn Fn(ObjectId) -> bool,
    remote: &Tree,
    same_here: &mut dyn FnMut(ObjectId, &Name, &Folder<'_>) -> Option<ObjectId>,
) -> Vec<Twin> {
    let mut here: HashMap<ObjectId, Folder<'_>> = HashMap::new();
    for (id, n) in local {
        for l in &n.links {
            here.entry(l.parent).or_default().insert(&l.name, *id);
        }
    }
    let empty = Folder::new();
    let mut counterpart: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
    let mut folders: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    for (id, n) in local {
        if *id == ObjectId::ROOT || known(*id) {
            continue;
        }
        for l in &n.links {
            if let Some(r) = server_folder(l.parent, local, known, remote, &mut counterpart) {
                folders.insert(l.parent, r);
            }
        }
    }
    let mut out: Vec<Twin> = Vec::new();
    let mut held: BTreeSet<ObjectId> = BTreeSet::new();
    for (folder, r) in folders {
        let in_it = here.get(&folder).unwrap_or(&empty);
        for (name, _) in remote.children(r) {
            if in_it.contains_key(name) {
                continue;
            }
            let Some(x) = same_here(folder, name, in_it) else {
                continue;
            };
            if known(x) || held.contains(&x) {
                continue;
            }
            // Its own name in this folder, which the server has nothing at.
            let at = local.get(&x).and_then(|n| {
                n.links
                    .iter()
                    .find(|l| l.parent == folder && l.name != *name)
            });
            if let Some(at) = at.filter(|l| remote.child(r, &l.name).is_none()) {
                held.insert(x);
                out.push(Twin {
                    object: x,
                    here: at.clone(),
                    server: name.clone(),
                });
            }
        }
    }
    out
}

/// The server's folder that a local one is, as the merge will match them: itself if the server has it,
/// or for a new one, the server's folder at its name inside its parent's.
fn server_folder(
    folder: ObjectId,
    local: &BTreeMap<ObjectId, LNode>,
    known: &dyn Fn(ObjectId) -> bool,
    remote: &Tree,
    memo: &mut HashMap<ObjectId, Option<ObjectId>>,
) -> Option<ObjectId> {
    if folder == ObjectId::ROOT {
        return Some(ObjectId::ROOT);
    }
    if let Some(r) = memo.get(&folder) {
        return *r;
    }
    // Nothing above it yet: a cycle cannot hang this.
    memo.insert(folder, None);
    let found = if remote
        .get_live(folder)
        .is_some_and(|n| n.kind() == ObjectKind::Dir)
    {
        Some(folder)
    } else if known(folder) {
        None
    } else {
        local
            .get(&folder)
            .and_then(|n| n.links.iter().next())
            .and_then(|l| {
                let up = server_folder(l.parent, local, known, remote, memo)?;
                let y = remote.child(up, &l.name)?;
                (remote.nodes[&y].kind() == ObjectKind::Dir && !local.contains_key(&y)).then_some(y)
            })
    };
    memo.insert(folder, found);
    found
}

/// Take what is held back out of what the merge sees, with everything inside it: what is new there
/// waits with it, and what the server knows stands as the server has it, as if this scan had not seen
/// it, so that nothing reads as deleted or moved.
pub fn hold_back(identified: &mut Identified, twins: &[Twin], remote: &Tree) {
    hold_back_objects(identified, twins.iter().map(|t| t.object).collect(), remote);
}

/// As `hold_back`, for any objects new here.
pub fn hold_back_objects(identified: &mut Identified, mut gone: BTreeSet<ObjectId>, remote: &Tree) {
    loop {
        let inside: Vec<ObjectId> = identified
            .local
            .iter()
            .filter(|(id, n)| {
                !gone.contains(id)
                    && !n.carried
                    && !n.links.is_empty()
                    && n.links.iter().all(|l| gone.contains(&l.parent))
            })
            .map(|(id, _)| *id)
            .collect();
        if inside.is_empty() {
            break;
        }
        gone.extend(inside);
    }
    for id in gone {
        let Some(n) = identified.local.remove(&id) else {
            continue;
        };
        identified.scanned.remove(&id);
        if let Some(r) = remote.get_live(id) {
            identified.local.insert(
                id,
                LNode {
                    links: r.links.clone(),
                    content: r.body.content.clone(),
                    stored: r.body.metadata.clone(),
                    apply_from: r.body.metadata.clone(),
                    carried: true,
                    conflict_of: n.conflict_of,
                },
            );
        }
    }
}

/// A name as a person can tell it from its twin: as it shows, and where that is not plain ASCII, by its
/// code points too, since a composed and a decomposed name look the same.
fn spelled(name: &Name) -> String {
    let shown = name.to_string();
    if shown.is_ascii() {
        return format!("`{shown}`");
    }
    let points: String = shown
        .chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_string()
            } else {
                format!("\\u{{{:x}}}", u32::from(c))
            }
        })
        .collect();
    format!("`{shown}` ({points})")
}

/// Before anything of it goes up: find what this filesystem takes for a name the server already has,
/// hold it back, and say so, with both names.
pub fn hold_back_twins(
    root: &Root,
    identified: &mut Identified,
    base: &Tree,
    remote: &Tree,
) -> Vec<Skip> {
    let known = |id: ObjectId| {
        base.nodes.contains_key(&id)
            || remote.nodes.contains_key(&id)
            || identified.shared.contains_key(&id)
    };
    if identified.local.keys().all(|id| known(*id)) {
        return Vec::new();
    }
    let scanned = &identified.scanned;
    let path_of = |id: ObjectId| scanned.get(&id).and_then(|s| s.paths.first());
    let mut dirs: HashMap<ObjectId, Option<crate::fsops::Dir>> = HashMap::new();
    let mut same_here = |folder: ObjectId, name: &Name, in_it: &Folder<'_>| -> Option<ObjectId> {
        let dir = dirs
            .entry(folder)
            .or_insert_with(|| {
                let path: &[Name] = if folder == ObjectId::ROOT {
                    &[]
                } else {
                    path_of(folder)?
                };
                root.dir.open_path(path).ok()
            })
            .as_ref()?;
        let st = dir.try_stat(name.as_bytes()).ok()??;
        in_it.values().copied().find(|id| {
            scanned
                .get(id)
                .is_some_and(|s| (s.fp.dev, s.fp.ino) == (st.dev, st.ino))
        })
    };
    let twins = find(&identified.local, &known, remote, &mut same_here);
    let skips = twins
        .iter()
        .map(|t| {
            let path = path_of(t.object)
                .cloned()
                .unwrap_or_else(|| vec![t.here.name.clone()]);
            Skip {
                path: crate::scan::display_path(&path),
                kind: SkipKind::SameNameOnServer,
                detail: Some(format!(
                    "on the server {}, here {}",
                    spelled(&t.server),
                    spelled(&t.here.name)
                )),
            }
        })
        .collect();
    hold_back(identified, &twins, remote);
    skips
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{Content, Hash, Metadata, Op, RevisionBody, RevisionId};

    fn name(s: &str) -> Name {
        Name::new(s.as_bytes().to_vec()).unwrap()
    }
    fn file(s: &str) -> Content {
        Content::File {
            manifest: Hash::of(s.as_bytes()),
            size: s.len() as u64,
        }
    }
    fn lnode(parent: ObjectId, n: &str, content: Content) -> LNode {
        LNode {
            links: [Link {
                parent,
                name: name(n),
            }]
            .into(),
            content,
            stored: Metadata::default(),
            apply_from: Metadata::default(),
            carried: false,
            conflict_of: None,
        }
    }

    const NFC: &str = "Caf\u{e9}";
    const NFD: &str = "Cafe\u{301}";

    /// APFS, by default: one name whatever the normalization, and whatever the case.
    fn apfs(s: &str) -> String {
        s.replace("e\u{301}", "\u{e9}").to_lowercase()
    }

    struct T {
        remote: Tree,
        local: BTreeMap<ObjectId, LNode>,
    }

    impl T {
        fn new() -> Self {
            let mut local = BTreeMap::new();
            local.insert(ObjectId::ROOT, lnode(ObjectId::ROOT, "x", Content::Dir));
            local.get_mut(&ObjectId::ROOT).unwrap().links.clear();
            Self {
                remote: Tree::new(),
                local,
            }
        }

        fn server(&mut self, parent: ObjectId, n: &str, content: Content) -> ObjectId {
            let id = ObjectId::random();
            self.remote
                .apply(&Op::Create {
                    object: id,
                    revision: RevisionId::random(),
                    body: RevisionBody {
                        content,
                        metadata: Metadata::default(),
                    },
                    link: Link {
                        parent,
                        name: name(n),
                    },
                    conflict_of: None,
                })
                .unwrap();
            id
        }

        fn here(&mut self, parent: ObjectId, n: &str, content: Content) -> ObjectId {
            let id = ObjectId::random();
            self.local.insert(id, lnode(parent, n, content));
            id
        }

        /// Twins as a filesystem that folds names with `fold` finds them.
        fn find(&self, fold: fn(&str) -> String) -> Vec<Twin> {
            let remote = &self.remote;
            let known = |id: ObjectId| remote.nodes.contains_key(&id);
            let mut same_here = |_: ObjectId, server: &Name, in_it: &Folder<'_>| {
                let want = fold(&server.to_string());
                in_it
                    .iter()
                    .find(|(n, _)| fold(&n.to_string()) == want)
                    .map(|(_, id)| *id)
            };
            find(&self.local, &known, &self.remote, &mut same_here)
        }
    }

    fn bytes(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn a_decomposed_name_new_here_beside_the_servers_composed_one_waits_with_all_in_it() {
        let mut t = T::new();
        let y = t.server(ObjectId::ROOT, NFC, Content::Dir);
        t.server(y, "menu.txt", file("soup"));
        let x = t.here(ObjectId::ROOT, NFD, Content::Dir);
        let inside = t.here(x, "menu.txt", file("soup"));
        let twins = t.find(apfs);
        assert_eq!(
            twins,
            vec![Twin {
                object: x,
                here: Link {
                    parent: ObjectId::ROOT,
                    name: name(NFD)
                },
                server: name(NFC),
            }]
        );

        let mut identified = Identified {
            local: t.local.clone(),
            ..Identified::default()
        };
        hold_back(&mut identified, &twins, &t.remote);
        assert!(!identified.local.contains_key(&x));
        assert!(
            !identified.local.contains_key(&inside),
            "it waits with its folder"
        );
        assert!(identified.local.contains_key(&ObjectId::ROOT));
    }

    #[test]
    fn a_name_in_another_case_waits_where_the_filesystem_folds_case() {
        let mut t = T::new();
        t.server(ObjectId::ROOT, "Readme.txt", file("x"));
        let x = t.here(ObjectId::ROOT, "README.txt", file("x"));
        let twins = t.find(apfs);
        assert_eq!(twins.len(), 1);
        assert_eq!(twins[0].object, x);
        assert_eq!(twins[0].server, name("Readme.txt"));
    }

    #[test]
    fn where_names_are_bytes_both_forms_are_names_of_their_own() {
        let mut t = T::new();
        t.server(ObjectId::ROOT, NFC, Content::Dir);
        t.server(ObjectId::ROOT, "Readme.txt", file("x"));
        t.here(ObjectId::ROOT, NFD, Content::Dir);
        t.here(ObjectId::ROOT, "README.txt", file("x"));
        assert!(t.find(bytes).is_empty());
    }

    /// The server may hold both forms, from a device where they are two names: the one in this form
    /// is the one this merges with, and the other cannot come here, as before.
    #[test]
    fn the_servers_name_in_this_very_form_is_what_it_merges_with() {
        let mut t = T::new();
        t.server(ObjectId::ROOT, "File.txt", file("1"));
        t.server(ObjectId::ROOT, "file.txt", file("2"));
        t.here(ObjectId::ROOT, "file.txt", file("2"));
        assert!(t.find(apfs).is_empty());
    }

    #[test]
    fn inside_a_folder_both_have_the_names_are_compared_too() {
        let mut t = T::new();
        let photos = t.server(ObjectId::ROOT, "Photos", Content::Dir);
        t.server(photos, &format!("{NFC}.jpg"), file("jpeg"));
        let here = t.here(ObjectId::ROOT, "Photos", Content::Dir);
        let x = t.here(here, &format!("{NFD}.jpg"), file("jpeg"));
        t.here(here, "only-here.jpg", file("other"));
        let twins = t.find(apfs);
        assert_eq!(twins.len(), 1, "{twins:?}");
        assert_eq!(twins[0].object, x);
        assert_eq!(twins[0].here.parent, here);
    }

    /// What the server already knows is not new, even renamed here to the twin of another name there.
    #[test]
    fn nothing_the_server_knows_is_held_back() {
        let mut t = T::new();
        t.server(ObjectId::ROOT, NFC, Content::Dir);
        let k = t.server(ObjectId::ROOT, "notes", Content::Dir);
        t.local.insert(k, lnode(ObjectId::ROOT, NFD, Content::Dir));
        t.here(ObjectId::ROOT, "new.txt", file("new"));
        assert!(t.find(apfs).is_empty());
    }

    /// Moved here into a folder that waits: it stays as the server has it rather than read as gone.
    #[test]
    fn what_the_server_knows_inside_a_folder_that_waits_stands_as_it_is_there() {
        let mut t = T::new();
        t.server(ObjectId::ROOT, NFC, Content::Dir);
        let k = t.server(ObjectId::ROOT, "notes.txt", file("n"));
        let x = t.here(ObjectId::ROOT, NFD, Content::Dir);
        t.local.insert(k, lnode(x, "notes.txt", file("n")));
        let twins = t.find(apfs);
        let mut identified = Identified {
            local: t.local.clone(),
            ..Identified::default()
        };
        hold_back(&mut identified, &twins, &t.remote);
        let carried = &identified.local[&k];
        assert!(carried.carried);
        assert_eq!(carried.links, t.remote.nodes[&k].links);
    }

    #[test]
    fn both_names_can_be_told_apart_in_the_report() {
        assert_eq!(spelled(&name("Readme.txt")), "`Readme.txt`");
        assert_eq!(spelled(&name(NFC)), "`Caf\u{e9}` (Caf\\u{e9})");
        assert_eq!(spelled(&name(NFD)), "`Cafe\u{301}` (Cafe\\u{301})");
    }
}
