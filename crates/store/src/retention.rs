//! The retention rule: which revisions a folder's policy gives up, as a pure function of its journal,
//! the policy and the time (docs/format.md, "Retention"). For a fixed policy it only ever gives up more
//! as time goes on, so any prefix of a pass that marks what it gives up leaves the store whole.
//!
//! Only revisions that name a manifest are given up: a file's, or one with streams. The journal, the
//! revisions' bodies, objects and entries all stay whole, so a device away for any length of time still
//! replays what it missed. Never given up:
//! - an object's head while it has an entry;
//! - a revision that was an object's head, while it had an entry, at a point a tier keeps (the start of
//!   each hour, day or Monday, UTC, in that tier's ages);
//! - a revision that was an object's head at any time in an `all` tier's ages, or was made then;
//! - a deleted object's last revision, until the trash period has passed since its delete.
//!
//! A point in time is the state after the last commit at or before it. The journal's times are the
//! server's, which a clock set back could make go backwards: each commit is taken at the latest time of
//! any commit up to it.

use anyhow::{anyhow, Result};
use cairn_protocol::retention::Policy;
use cairn_protocol::wire::Change;
use cairn_protocol::{Content, ObjectId, Op, RevisionBody, RevisionId};
use std::collections::{HashMap, HashSet};

/// Whether a revision names content a collection could take: its file's manifest, or a stream's.
pub fn names_manifest(body: &RevisionBody) -> bool {
    matches!(body.content, Content::File { .. }) || !body.metadata.streams.is_empty()
}

/// An object as the replay has it: its head, its entries, and the stretch of time since its head or
/// whether it has an entry last changed.
struct Obj {
    head: RevisionId,
    links: u32,
    since: i64,
    seg_head: RevisionId,
    seg_linked: bool,
}

/// The rule, fed a namespace's journal in order and asked at its end.
pub struct Rule {
    now: i64,
    /// Times every revision of is kept, `[lo, hi]`.
    all: Vec<(i128, i128)>,
    /// Points the state is kept at: every `unit` seconds from `offset`, in `[lo, hi]`.
    points: Vec<(i128, i128, i128, i128)>,
    trash: i64,
    clock: i64,
    objects: HashMap<ObjectId, Obj>,
    candidates: HashSet<RevisionId>,
    kept: HashSet<RevisionId>,
}

/// What the rule found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The revisions given up, in no order.
    pub prune: Vec<RevisionId>,
    /// The revisions that name a manifest, and of them those kept.
    pub considered: usize,
    pub kept: usize,
}

impl Rule {
    pub fn new(policy: &Policy, now: i64) -> Self {
        let mut all = Vec::new();
        let mut points = Vec::new();
        let n = i128::from(now);
        for (every, from, to) in policy.ages() {
            // Ages `[from, to)` are the times `(now - to, now - from]`.
            let hi = n - i128::from(from);
            let lo = to.map_or(i128::MIN / 4, |to| n - i128::from(to) + 1);
            match every.grid() {
                None => all.push((lo, hi)),
                Some((unit, offset)) => {
                    points.push((i128::from(unit), i128::from(offset), lo, hi));
                }
            }
        }
        let mut objects = HashMap::new();
        // The root, made with the namespace, has no entry and is never deleted.
        objects.insert(
            ObjectId::ROOT,
            Obj {
                head: RevisionId::ZERO,
                links: 1,
                since: i64::MIN,
                seg_head: RevisionId::ZERO,
                seg_linked: true,
            },
        );
        Rule {
            now,
            all,
            points,
            trash: i64::try_from(policy.trash).unwrap_or(i64::MAX),
            clock: i64::MIN,
            objects,
            candidates: HashSet::new(),
            kept: HashSet::new(),
        }
    }

    fn in_all(&self, t: i64) -> bool {
        let t = i128::from(t);
        self.all.iter().any(|&(lo, hi)| lo <= t && t <= hi)
    }

    /// Whether the state over `[a, b)` holds a point a tier keeps, or lies in an `all` tier's ages.
    fn kept_over(&self, a: i64, b: i64) -> bool {
        if a >= b {
            return false;
        }
        let (a, last) = (i128::from(a), i128::from(b) - 1);
        if self.all.iter().any(|&(lo, hi)| lo.max(a) <= hi.min(last)) {
            return true;
        }
        self.points.iter().any(|&(unit, offset, lo, hi)| {
            let (from, to) = (lo.max(a), hi.min(last));
            from <= to && from + (offset - from).rem_euclid(unit) <= to
        })
    }

    fn revision(&mut self, revision: RevisionId, body: &RevisionBody, t: i64) {
        if names_manifest(body) {
            self.candidates.insert(revision);
            if self.in_all(t) {
                self.kept.insert(revision);
            }
        }
    }

    /// One commit, as the store applied it: anything it could not have is a journal that does not
    /// replay, and nothing is given up of it.
    pub fn apply(&mut self, change: &Change) -> Result<()> {
        let t = self.clock.max(change.time);
        self.clock = t;
        let corrupt = |o: &ObjectId| {
            anyhow!(
                "the journal does not replay at seq {}: object {o}",
                change.seq
            )
        };
        let mut touched: Vec<ObjectId> = Vec::new();
        for op in &change.ops {
            let object = match op {
                Op::Create {
                    object,
                    revision,
                    body,
                    ..
                } => {
                    if self.objects.contains_key(object) {
                        return Err(corrupt(object));
                    }
                    self.objects.insert(
                        *object,
                        Obj {
                            head: *revision,
                            links: 1,
                            since: t,
                            seg_head: *revision,
                            seg_linked: true,
                        },
                    );
                    self.revision(*revision, body, t);
                    object
                }
                Op::Update {
                    object,
                    revision,
                    body,
                    ..
                } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    o.head = *revision;
                    self.revision(*revision, body, t);
                    object
                }
                Op::Link { object, .. } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    o.links += 1;
                    object
                }
                Op::Unlink { object, .. } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    o.links = o.links.checked_sub(1).ok_or_else(|| corrupt(object))?;
                    object
                }
                Op::Move { object, .. } => {
                    if !self.objects.contains_key(object) {
                        return Err(corrupt(object));
                    }
                    continue;
                }
                Op::Note { .. } => continue,
            };
            touched.push(*object);
        }
        // Only the state after the commit is a state of the tree.
        for object in touched {
            let Some(o) = self.objects.get(&object) else {
                continue;
            };
            let (head, linked) = (o.head, o.links > 0);
            if (o.seg_head, o.seg_linked) == (head, linked) {
                continue;
            }
            let (since, seg_head, seg_linked) = (o.since, o.seg_head, o.seg_linked);
            if seg_linked && self.kept_over(since, t) {
                self.kept.insert(seg_head);
            }
            let o = self.objects.get_mut(&object).expect("looked up above");
            o.since = t;
            o.seg_head = head;
            o.seg_linked = linked;
        }
        Ok(())
    }

    /// What the journal so far gives up, as of `now`.
    pub fn finish(mut self) -> Outcome {
        let now = self.now;
        let ends: Vec<(RevisionId, bool, i64)> = self
            .objects
            .values()
            .map(|o| (o.seg_head, o.seg_linked, o.since))
            .collect();
        for (head, linked, since) in ends {
            // A head with an entry, always; a deleted one's within its trash period.
            if linked || now.saturating_sub(since) < self.trash {
                self.kept.insert(head);
            }
        }
        let prune: Vec<RevisionId> = self
            .candidates
            .iter()
            .filter(|r| !self.kept.contains(r))
            .copied()
            .collect();
        Outcome {
            considered: self.candidates.len(),
            kept: self.candidates.len() - prune.len(),
            prune,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::retention::{DAY, HOUR};
    use cairn_protocol::{DeviceId, Hash, Link, Metadata, Name, OpId};
    use std::collections::BTreeSet;

    fn file(n: u64) -> RevisionBody {
        RevisionBody {
            content: Content::File {
                manifest: Hash::of(&n.to_le_bytes()),
                size: n,
            },
            metadata: Metadata::default(),
        }
    }

    fn link(n: u64) -> Link {
        Link {
            parent: ObjectId::ROOT,
            name: Name::new(format!("f{n}").into_bytes()).unwrap(),
        }
    }

    fn change(seq: u64, time: i64, ops: Vec<Op>) -> Change {
        Change {
            seq,
            op_id: OpId::random(),
            device: DeviceId::random(),
            device_name: "d".into(),
            time,
            ops,
            user: None,
        }
    }

    /// A journal: each step a time and its operations.
    struct J {
        changes: Vec<Change>,
        next: u64,
    }

    impl J {
        fn new() -> Self {
            J {
                changes: Vec::new(),
                next: 1,
            }
        }
        fn at(&mut self, time: i64, ops: Vec<Op>) {
            let seq = self.changes.len() as u64 + 1;
            self.changes.push(change(seq, time, ops));
        }
        fn create(&mut self, time: i64) -> (ObjectId, RevisionId) {
            let (o, r) = (ObjectId::random(), RevisionId::random());
            let n = self.next;
            self.next += 1;
            self.at(
                time,
                vec![Op::Create {
                    object: o,
                    revision: r,
                    body: file(n),
                    link: link(n),
                    conflict_of: None,
                }],
            );
            (o, r)
        }
        fn update(&mut self, time: i64, o: ObjectId, base: RevisionId) -> RevisionId {
            let r = RevisionId::random();
            let n = self.next;
            self.next += 1;
            self.at(
                time,
                vec![Op::Update {
                    object: o,
                    base,
                    revision: r,
                    body: file(n),
                    restored_from: None,
                }],
            );
            r
        }
        fn delete(&mut self, time: i64, o: ObjectId, n: u64) {
            self.at(
                time,
                vec![Op::Unlink {
                    object: o,
                    link: link(n),
                    base: None,
                }],
            );
        }
        fn pruned(&self, policy: &str, now: i64) -> BTreeSet<RevisionId> {
            let policy: Policy = policy.parse().unwrap();
            let mut rule = Rule::new(&policy, now);
            for c in &self.changes {
                rule.apply(c).unwrap();
            }
            rule.finish().prune.into_iter().collect()
        }
    }

    /// 2026-11-16 00:00 UTC, a Monday.
    const MONDAY: i64 = 1_794_787_200;
    const H: i64 = HOUR as i64;
    const D: i64 = DAY as i64;

    #[test]
    fn a_tier_keeps_the_head_at_each_of_its_points_and_no_further_back() {
        let mut j = J::new();
        let (o, r1) = j.create(MONDAY + 10 * H + 1800);
        let r2 = j.update(MONDAY + 11 * H + 1800, o, r1);
        // r1 was the head at 11:00. Its age passes the day the tier covers a second after 11:00 the
        // next day.
        let edge = MONDAY + D + 11 * H;
        assert!(j.pruned("hourly 1d", edge - 1).is_empty());
        assert_eq!(j.pruned("hourly 1d", edge), [r1].into());
        // The head is never given up.
        assert!(!j.pruned("hourly 1d", edge + 100 * D).contains(&r2));
        // A revision that was the head at no hour's start is given up whatever its age, outside `all`.
        let r3 = j.update(MONDAY + 11 * H + 1801, o, r2);
        let _r4 = j.update(MONDAY + 11 * H + 1802, o, r3);
        assert!(j.pruned("hourly 1d", MONDAY + 12 * H).contains(&r3));
        assert!(!j.pruned("all 1d", MONDAY + 12 * H).contains(&r3));
        // `all` ends where its span does: a revision made then and replaced within it is given up
        // once its time is past the tier.
        assert!(j.pruned("all 1h", MONDAY + 13 * H).contains(&r3));
        // The second tier starts where the first ends: hourly from an hour back.
        let both = j.pruned("all 1h, hourly 1d", MONDAY + 13 * H);
        assert!(
            !both.contains(&r1) && both.contains(&r2),
            "r1 was the head at 11:00, r2 at no hour"
        );
    }

    #[test]
    fn a_week_is_kept_at_monday_utc() {
        let mut j = J::new();
        // Head from Sunday to Tuesday: across Monday 00:00.
        let (o, r1) = j.create(MONDAY - D);
        let r2 = j.update(MONDAY + D, o, r1);
        // Head from Tuesday to Sunday: no Monday.
        let r3 = j.update(MONDAY + 6 * D, o, r2);
        let now = MONDAY + 400 * D;
        let pruned = j.pruned("weekly forever", now);
        assert!(!pruned.contains(&r1));
        assert!(pruned.contains(&r2));
        assert!(!pruned.contains(&r3), "the head");
        // Daily keeps both, while it covers them.
        assert!(j.pruned("daily 30d", MONDAY + 20 * D).is_empty());
    }

    #[test]
    fn a_deleted_files_last_revision_stays_its_trash_period() {
        let mut j = J::new();
        // Made late on Sunday: the head at Monday 00:00.
        let (o, r1) = j.create(MONDAY - H);
        j.delete(MONDAY + 2 * H, o, 1);
        let deleted = MONDAY + 2 * H;
        assert!(j.pruned("trash 7d", deleted + 7 * D - 1).is_empty());
        assert_eq!(j.pruned("trash 7d", deleted + 7 * D), [r1].into());
        // Kept beyond it where a tier keeps the state it had while it had an entry.
        assert!(j.pruned("daily 30d; trash 7d", deleted + 7 * D).is_empty());
        let (o2, r2) = j.create(MONDAY + 3 * H);
        j.delete(MONDAY + 4 * H, o2, 2);
        // Never at a day's start: given up with its trash.
        assert!(j
            .pruned("daily 30d; trash 7d", MONDAY + 4 * H + 7 * D)
            .contains(&r2));
        // Brought back, it is a head again.
        j.at(
            MONDAY + 5 * H,
            vec![Op::Link {
                object: o2,
                link: link(2),
            }],
        );
        assert!(!j.pruned("trash 7d", MONDAY + 100 * D).contains(&r2));
    }

    #[test]
    fn a_directorys_revisions_and_a_clock_set_back_are_no_matter() {
        let mut j = J::new();
        let d = ObjectId::random();
        j.at(
            MONDAY,
            vec![Op::Create {
                object: d,
                revision: RevisionId::random(),
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: Metadata::default(),
                },
                link: link(99),
                conflict_of: None,
            }],
        );
        let (o, r1) = j.create(MONDAY + H + 10);
        // The clock set back: this commit is taken at the latest time before it.
        let r2 = j.update(MONDAY - 5 * D, o, r1);
        assert_eq!(j.pruned("hourly 1d", MONDAY + 3 * H), [r1].into());
        let _ = r2;
    }

    /// Small random journals of files made, changed, deleted and brought back, over days.
    fn random_journal(seed: u64) -> (J, Vec<(i64, usize)>) {
        let mut rng = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n.max(1)
        };
        let mut j = J::new();
        let mut files: Vec<(ObjectId, RevisionId, u64, bool)> = Vec::new();
        let mut t = MONDAY - 20 * D + next(86_400) as i64;
        let mut marks = Vec::new();
        for _ in 0..(20 + next(60)) {
            t += next(9 * 3600) as i64;
            match next(5) {
                0 | 1 if !files.is_empty() => {
                    let i = next(files.len() as u64) as usize;
                    let (o, r, n, live) = files[i];
                    if live {
                        let r2 = j.update(t, o, r);
                        files[i].1 = r2;
                    } else {
                        let _ = n;
                    }
                }
                2 if !files.is_empty() => {
                    let i = next(files.len() as u64) as usize;
                    let (o, _, n, live) = files[i];
                    if live {
                        j.delete(t, o, n);
                        files[i].3 = false;
                    } else {
                        j.at(
                            t,
                            vec![Op::Link {
                                object: o,
                                link: link(n),
                            }],
                        );
                        files[i].3 = true;
                    }
                }
                _ => {
                    let n = j.next;
                    let (o, r) = j.create(t);
                    files.push((o, r, n, true));
                }
            }
            marks.push((t, j.changes.len()));
        }
        (j, marks)
    }

    fn heads(j: &J, upto: usize) -> BTreeSet<RevisionId> {
        let mut head: HashMap<ObjectId, (RevisionId, u32)> = HashMap::new();
        for c in &j.changes[..upto] {
            for op in &c.ops {
                match op {
                    Op::Create {
                        object, revision, ..
                    } => {
                        head.insert(*object, (*revision, 1));
                    }
                    Op::Update {
                        object, revision, ..
                    } => head.get_mut(object).unwrap().0 = *revision,
                    Op::Link { object, .. } => head.get_mut(object).unwrap().1 += 1,
                    Op::Unlink { object, .. } => head.get_mut(object).unwrap().1 -= 1,
                    _ => {}
                }
            }
        }
        head.values()
            .filter(|(_, l)| *l > 0)
            .map(|(r, _)| *r)
            .collect()
    }

    #[test]
    fn over_random_journals_heads_stay_and_what_is_given_up_only_grows() {
        let policies = [
            "all 2d, hourly 1d, daily 30d, weekly forever; trash 30d",
            "hourly 1d, daily 7d; trash 1d",
            "all 12h; trash 2d",
            "daily 3d, weekly 4w",
            "trash 1d",
        ];
        for seed in 0..60 {
            let (j, marks) = random_journal(seed);
            for policy in policies {
                // As time goes on over a whole journal.
                let end = marks.last().unwrap().0;
                let mut before = BTreeSet::new();
                for step in 0..40 {
                    let now = end + step * 7 * H;
                    let pruned = j.pruned(policy, now);
                    assert!(
                        pruned.is_superset(&before),
                        "seed {seed}, {policy}: gave back {:?}",
                        before.difference(&pruned).collect::<Vec<_>>()
                    );
                    assert!(
                        pruned.is_disjoint(&heads(&j, j.changes.len())),
                        "seed {seed}, {policy}"
                    );
                    before = pruned;
                }
                // And as the journal grows, asked at each commit's time: what it gave up stays given
                // up, but for what was brought back since, which the store refuses once given up.
                let mut before = BTreeSet::new();
                // Brought back once given up, which the store refuses: no longer this test's to follow.
                let mut revived = BTreeSet::new();
                for &(t, upto) in &marks {
                    let prefix = J {
                        changes: j.changes[..upto].to_vec(),
                        next: 0,
                    };
                    let pruned = prefix.pruned(policy, t);
                    let live = heads(&j, upto);
                    assert!(pruned.is_disjoint(&live), "seed {seed}, {policy}");
                    revived.extend(before.intersection(&live).copied());
                    let gave_back: Vec<_> = before
                        .difference(&pruned)
                        .filter(|r| !revived.contains(*r))
                        .collect();
                    assert!(gave_back.is_empty(), "seed {seed}, {policy}: {gave_back:?}");
                    before = pruned;
                }
            }
        }
    }
}
