//! Randomized operations on several devices, with crashes, checked against the invariants.
//!
//! Every write is a unique self-checking version. A version is "superseded" only when a device that
//! could see it replaced or deleted it. After everything converges, every version not superseded
//! must exist somewhere in the tree: that is "arrival order never decides which content survives".
//!
//! `CAIRN_SEEDS` sets how many seeds run, `CAIRN_SEED` runs one to reproduce a failure.

mod common;
use common::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1000);

/// Stops a background mutator however its scope ends, so a failed assertion reports instead of
/// waiting forever on the thread.
struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn fresh(size: usize) -> (u64, Vec<u8>) {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    (id, token::make(id, size))
}

fn pick_size(rng: &mut StdRng) -> usize {
    match rng.random_range(0..20) {
        0 => rng.random_range(600_000..3_000_000),
        1..=3 => rng.random_range(4_000..60_000),
        _ => rng.random_range(30..300),
    }
}

/// Entries under a root, without the state directory.
fn entries(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(list) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in list.flatten() {
            if d == root && e.file_name() == ".cairn" {
                continue;
            }
            // A killed sync's leftover is the engine's, not something a user works on.
            if e.file_name().to_string_lossy().starts_with(".cairn-tmp-") {
                continue;
            }
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                dirs.push(e.path());
                stack.push(e.path());
            } else if t.is_file() {
                files.push(e.path());
            }
        }
    }
    files.sort();
    dirs.sort();
    (files, dirs)
}

fn token_of(path: &Path) -> Option<u64> {
    token::parse(&std::fs::read(path).ok()?)
}

/// Everything a device's user might do, recording which versions they knowingly replaced.
fn mutate(
    rng: &mut StdRng,
    d: &Device,
    live: &mut BTreeSet<u64>,
    named_away: &mut BTreeSet<u64>,
    log: &mut Vec<String>,
) {
    let (files, dirs) = entries(&d.root);
    let rel = |p: &Path| p.strip_prefix(&d.root).unwrap_or(p).display().to_string();
    let names = ["a", "b", "c", "d", "e", "f.txt", "g.tar.gz", ".hidden"];
    let name = names[rng.random_range(0..names.len())];
    let some_dir = |rng: &mut StdRng| -> PathBuf {
        if dirs.is_empty() || rng.random_bool(0.4) {
            d.root.clone()
        } else {
            dirs[rng.random_range(0..dirs.len())].clone()
        }
    };
    match rng.random_range(0..100) {
        // A new file.
        0..=24 => {
            let path = some_dir(rng).join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                return;
            }
            let size = pick_size(rng);
            let (id, data) = fresh(size);
            std::fs::write(&path, data).unwrap();
            live.insert(id);
            log.push(format!("{} new {} = {id}", d.name, rel(&path)));
        }
        // Edit in place.
        25..=44 if !files.is_empty() => {
            let path = &files[rng.random_range(0..files.len())];
            let old = token_of(path);
            if let Some(old) = old {
                live.remove(&old);
            }
            let size = pick_size(rng);
            let (id, data) = fresh(size);
            make_writable(path);
            std::fs::write(path, data).unwrap();
            live.insert(id);
            log.push(format!("{} edit {} {old:?} -> {id}", d.name, rel(path)));
        }
        // Save as editors do: write aside, rename over.
        45..=54 if !files.is_empty() => {
            let path = &files[rng.random_range(0..files.len())];
            let tmp = path.with_extension("editor-save");
            if std::fs::symlink_metadata(&tmp).is_ok() {
                return;
            }
            let old = token_of(path);
            if let Some(old) = old {
                if nlink(path) == 1 {
                    live.remove(&old);
                } else {
                    named_away.insert(old);
                }
            }
            let size = pick_size(rng);
            let (id, data) = fresh(size);
            std::fs::write(&tmp, data).unwrap();
            make_writable(path);
            std::fs::rename(&tmp, path).unwrap();
            live.insert(id);
            log.push(format!("{} save {} {old:?} -> {id}", d.name, rel(path)));
        }
        // Delete a file.
        55..=64 if !files.is_empty() => {
            let path = &files[rng.random_range(0..files.len())];
            let old = token_of(path);
            let links = nlink(path);
            if let Some(old) = old {
                if links == 1 {
                    live.remove(&old);
                } else {
                    named_away.insert(old);
                }
            }
            remove_file(path);
            log.push(format!(
                "{} rm {} {old:?} (nlink {links})",
                d.name,
                rel(path)
            ));
        }
        // Rename a file or directory somewhere.
        65..=74 => {
            let all: Vec<&PathBuf> = files.iter().chain(dirs.iter()).collect();
            if all.is_empty() {
                return;
            }
            let from = all[rng.random_range(0..all.len())];
            let to = some_dir(rng).join(format!("{name}-mv"));
            if std::fs::symlink_metadata(&to).is_ok() || to.starts_with(from) {
                return;
            }
            std::fs::rename(from, &to).unwrap();
            log.push(format!("{} mv {} {}", d.name, rel(from), rel(&to)));
        }
        // A directory.
        75..=84 => {
            let path = some_dir(rng).join(format!("dir-{name}"));
            let _ = std::fs::create_dir(path);
        }
        // Delete a whole directory.
        85..=89 if !dirs.is_empty() => {
            let dir = &dirs[rng.random_range(0..dirs.len())];
            let (inner, _) = entries(dir);
            // A version goes only if every link to it is inside this directory.
            let mut inside: std::collections::BTreeMap<u64, u64> =
                std::collections::BTreeMap::new();
            for f in &inner {
                *inside.entry(inode(f)).or_default() += 1;
            }
            let mut gone = Vec::new();
            for f in &inner {
                let ino = inode(f);
                if let Some(old) = token_of(f) {
                    if inside[&ino] == nlink(f) {
                        live.remove(&old);
                        gone.push(old);
                    } else {
                        named_away.insert(old);
                    }
                }
                make_writable(f);
            }
            std::fs::remove_dir_all(dir).unwrap();
            log.push(format!("{} rm -r {} {gone:?}", d.name, rel(dir)));
        }
        // chmod, xattr. Windows holds a file's write bits alone, and no xattrs.
        90..=93 if !files.is_empty() => {
            let path = &files[rng.random_range(0..files.len())];
            let mode = [0o644, 0o600, 0o755, 0o640][rng.random_range(0..4)];
            chmod(
                path,
                if mode == 0o644 {
                    mode
                } else {
                    other_mode(mode)
                },
            );
        }
        #[cfg(unix)]
        94..=95 if !files.is_empty() => {
            let path = &files[rng.random_range(0..files.len())];
            set_xattr(
                path,
                "user.tag",
                format!("{}", rng.random_range(0..5)).as_bytes(),
            );
        }
        // A hard link.
        96..=97 if !files.is_empty() => {
            let from = &files[rng.random_range(0..files.len())];
            let to = some_dir(rng).join(format!("{name}-ln"));
            if std::fs::symlink_metadata(&to).is_err() {
                std::fs::hard_link(from, &to).unwrap();
                log.push(format!("{} ln {} {}", d.name, rel(from), rel(&to)));
            }
        }
        // A symlink, sometimes dangling, sometimes leaving the tree. Windows holds none yet.
        #[cfg(unix)]
        98..=99 => {
            let to = some_dir(rng).join(format!("{name}-sym"));
            if std::fs::symlink_metadata(&to).is_err() {
                let target = ["a", "../..", "/etc/hostname", "no/such"][rng.random_range(0..4)];
                std::os::unix::fs::symlink(target, to).unwrap();
            }
        }
        _ => {}
    }
}

/// What one device leaves out in the variant with rules: names `mutate` really uses.
const RULES: &str = "f.txt\ndir-a/\n*.tar.gz\n*-mv\n!a-mv\n";

fn run_seed(seed: u64, with_rules: bool) {
    let mut rng = StdRng::seed_from_u64(seed);
    let sb = Sandbox::fast("random");
    let mut server = Server::start(sb.path("server"));
    let devices: Vec<Device> = (0..3)
        .map(|i| Device::new(&sb, &server, &format!("d{i}")))
        .collect();
    let mut live = BTreeSet::new();
    // Versions with a hard link knowingly removed on some device. If other devices knowingly removed
    // the other names, all of them are gone, and no one edited it: a delete, not a loss.
    let mut named_away = BTreeSet::new();
    let steps = rng.random_range(30..70);
    let mut log: Vec<String> = Vec::new();
    let context = |msg: String| format!("seed {seed}: {msg}");
    let rules_file = devices[0].path(".cairn/ignore");
    if with_rules {
        std::fs::write(&rules_file, RULES).unwrap();
    }

    for step in 0..steps {
        let d = &devices[rng.random_range(0..devices.len())];
        // A share of the syncs, fixed by the seed and the step and drawing nothing from the steps' own
        // randomness, rebuild the ledger from the state, which the oracle compares with the full sync.
        let rebuild: &[(&str, &str)] =
            match (seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ step as u64).count_ones() % 3 {
                0 => &[("CAIRN_FP_LEDGER_REBUILD", "1")],
                _ => &[],
            };
        let mark = if rebuild.is_empty() {
            ""
        } else {
            " (ledger rebuilt)"
        };
        // The rules come and go: everything at those paths is left out, then merged back in.
        if with_rules && rng.random_bool(0.08) {
            let on = std::fs::read(&rules_file).is_ok_and(|t| !t.is_empty());
            std::fs::write(&rules_file, if on { "" } else { RULES }).unwrap();
            log.push(format!("d0 rules {}", if on { "off" } else { "on" }));
            continue;
        }
        match rng.random_range(0..100) {
            0..=59 => {
                for _ in 0..rng.random_range(1..4) {
                    mutate(&mut rng, d, &mut live, &mut named_away, &mut log);
                }
            }
            // A sync while something here cannot be read, the most common thing to go unobserved.
            60..=64 if !is_root() => {
                let (files, dirs) = entries(&d.root);
                let all: Vec<&PathBuf> = files.iter().chain(dirs.iter()).collect();
                if all.is_empty() {
                    continue;
                }
                let target = all[rng.random_range(0..all.len())];
                let (mode, ino) = (mode_of(target), inode(target));
                chmod(target, 0o000);
                let run = d.cairn_env(&["sync", "--allow-mass-delete"], rebuild);
                // Another device may have moved it meanwhile: a rename needs no access to what moves.
                let (files, dirs) = entries(&d.root);
                let now = files
                    .iter()
                    .chain(dirs.iter())
                    .find(|p| std::fs::symlink_metadata(p).is_ok() && inode(p) == ino);
                if let Some(now) = now {
                    chmod(now, mode);
                }
                log.push(format!(
                    "{} sync{mark} with {} unreadable -> {:?}: {}",
                    d.name,
                    target.strip_prefix(&d.root).unwrap().display(),
                    run.code,
                    run.stdout.trim().replace('\n', " | ")
                ));
                assert!(
                    run.ok() || run.code == Some(3),
                    "{}",
                    context(format!("step {step}: {}\n{}", run.text(), log.join("\n")))
                );
            }
            60..=84 => {
                let run = d.cairn_env(&["sync", "--allow-mass-delete"], rebuild);
                log.push(format!(
                    "{} sync{mark} -> {:?}: {}",
                    d.name,
                    run.code,
                    run.stdout.trim().replace('\n', " | ")
                ));
                assert!(
                    run.ok() || run.code == Some(3),
                    "{}",
                    context(format!("step {step}: {}\n{}", run.text(), log.join("\n")))
                );
            }
            // Killed partway; the next run recovers.
            85..=94 => {
                let crash = rng.random_range(1..150).to_string();
                let mut env = vec![("CAIRN_FP_CRASH", crash.as_str())];
                env.extend(rebuild);
                let run = d.cairn_env(&["sync", "--allow-mass-delete"], &env);
                log.push(format!(
                    "{} sync{mark} killed at {crash} -> {:?}",
                    d.name, run.code
                ));
                assert!(
                    run.was_killed() || run.ok() || run.code == Some(3),
                    "{}",
                    context(format!("{}\n{}", run.text(), log.join("\n")))
                );
            }
            // The server dies mid-sync, and comes back.
            _ => {
                let crash = rng.random_range(1..40).to_string();
                server.set_env(vec![("CAIRN_FP_CRASH".into(), crash.clone())]);
                server.restart();
                let run = d.cairn(&["sync", "--allow-mass-delete"]);
                log.push(format!(
                    "{} sync, server killed at {crash} -> {:?}",
                    d.name, run.code
                ));
                server.set_env(Vec::new());
                server.restart();
            }
        }
    }

    // Beside the trees, for a failure below, whose message does not carry it.
    std::fs::write(sb.path("steps.log"), log.join("\n") + "\n").unwrap();

    // Whatever was left out, all of it comes back in here.
    if with_rules {
        std::fs::write(&rules_file, "").unwrap();
    }
    let refs: Vec<&Device> = devices.iter().collect();
    let p = converge(&refs);
    let found = token::all_in(&p);
    let lost: Vec<_> = live
        .difference(&found)
        .filter(|v| !named_away.contains(v))
        .collect();
    assert!(
        lost.is_empty(),
        "{}",
        context(format!(
            "versions lost: {lost:?}\nnames: {:?}\n{}",
            p.names(),
            log.join("\n")
        ))
    );
    for d in &devices {
        assert!(
            leftover_temps(&d.root).is_empty(),
            "{}",
            context("temporary files left".into())
        );
    }

    // On the trees these steps made, every device's syncs settle into one with nothing to do, which
    // the full sync run after it in this build must agree with. d0's may stay with the full sync,
    // where its rules kept something out by inode, which only the full sync can tell.
    for (i, d) in devices.iter().enumerate() {
        let reached = settles_fast(d);
        assert!(
            reached || (with_rules && i == 0),
            "{}",
            context(format!(
                "{} never took a sync for one with nothing to do",
                d.name
            ))
        );
    }
    // Then a change on each, which the fast path must leave to the full sync, and which survives like
    // any other.
    for d in &devices {
        mutate(&mut rng, d, &mut live, &mut named_away, &mut log);
    }
    let p = converge(&refs);
    let found = token::all_in(&p);
    let lost: Vec<_> = live
        .difference(&found)
        .filter(|v| !named_away.contains(v))
        .collect();
    assert!(
        lost.is_empty(),
        "{}",
        context(format!(
            "versions lost after a sync with nothing to do: {lost:?}\n{}",
            log.join("\n")
        ))
    );
    let report = server.fsck();
    assert!(report.contains("clean"), "{}", context(report));

    // And once more a change on each, which every device's next sync takes up by what changed alone,
    // the full round beside each of its rounds agreeing. d0 may take the whole tree where its rules
    // keep something out by inode, which only the whole tree can tell.
    for d in &devices {
        mutate(&mut rng, d, &mut live, &mut named_away, &mut log);
    }
    for (i, d) in devices.iter().enumerate() {
        let (rounds, confirmed) = takes_part(d);
        assert!(
            rounds > 0 || (with_rules && i == 0),
            "{}",
            context(format!("{} took no round by what changed alone", d.name))
        );
        assert_eq!(rounds, confirmed, "{}", context(d.name.clone()));
    }
    let p = converge(&refs);
    let found = token::all_in(&p);
    let lost: Vec<_> = live
        .difference(&found)
        .filter(|v| !named_away.contains(v))
        .collect();
    assert!(
        lost.is_empty(),
        "{}",
        context(format!(
            "versions lost after syncs by what changed: {lost:?}\n{}",
            log.join("\n")
        ))
    );
}

/// One sync, and how many of its rounds took what changed alone, and how many of those the full round
/// beside them confirmed.
fn takes_part(d: &Device) -> (usize, usize) {
    let log = d
        .root
        .with_extension(format!("steps-{:016x}", rand::random::<u64>()));
    let run = d.cairn_env(
        &["sync", "--allow-mass-delete"],
        &[("CAIRN_FP_LOG", log.to_str().unwrap())],
    );
    let steps = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    assert!(run.ok() || run.code == Some(3), "{}", run.text());
    let count = |name: &str| steps.lines().filter(|l| *l == name).count();
    (count("sync.incr.round"), count("sync.incr.confirmed"))
}

/// Syncs until one is found to have nothing to do, and the full sync after it agrees.
fn settles_fast(d: &Device) -> bool {
    for _ in 0..4 {
        let log = d
            .root
            .with_extension(format!("steps-{:016x}", rand::random::<u64>()));
        let run = d.cairn_env(
            &["sync", "--allow-mass-delete"],
            &[("CAIRN_FP_LOG", log.to_str().unwrap())],
        );
        let steps = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        assert!(run.ok() || run.code == Some(3), "{}", run.text());
        if steps.lines().any(|l| l == "sync.fast.confirmed") {
            return true;
        }
    }
    false
}

#[test]
fn random_operations_on_three_devices_lose_nothing() {
    many_seeds(false);
}

#[test]
fn random_operations_with_rules_coming_and_going_on_one_device_lose_nothing() {
    many_seeds(true);
}

fn many_seeds(with_rules: bool) {
    if let Ok(seed) = std::env::var("CAIRN_SEED") {
        run_seed(seed.parse().expect("CAIRN_SEED is a number"), with_rules);
        return;
    }
    let seeds: u64 = std::env::var("CAIRN_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    let next = AtomicU64::new(0);
    let failed = std::sync::Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(12);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let seed = next.fetch_add(1, Ordering::SeqCst);
                if seed >= seeds {
                    return;
                }
                if let Err(e) = std::panic::catch_unwind(|| run_seed(seed, with_rules)) {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
                        .unwrap_or_default();
                    failed.lock().unwrap().push((seed, msg));
                }
            });
        }
    });
    let failed = failed.into_inner().unwrap();
    assert!(
        failed.is_empty(),
        "{} of {seeds} seeds failed; first: {:?}",
        failed.len(),
        failed.first()
    );
}

/// A rename storm while syncs run: a scan may see any mid-state of it, and none of it may lose a file
/// or look like a delete.
#[test]
fn a_rename_storm_during_sync_loses_nothing() {
    let sb = Sandbox::fast("storm");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    let mut live = BTreeSet::new();
    for i in 0..40 {
        let (id, data) = fresh(200);
        a.write(&format!("d{}/f{i}", i % 4), data);
        live.insert(id);
    }
    a.synced();
    b.synced();

    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let root = a.root.clone();
        let stop = &stop;
        s.spawn(move || {
            let mut rng = StdRng::seed_from_u64(99);
            while !stop.load(Ordering::SeqCst) {
                let (files, dirs) = entries(&root);
                if files.is_empty() || dirs.is_empty() {
                    continue;
                }
                let from = &files[rng.random_range(0..files.len())];
                let to = dirs[rng.random_range(0..dirs.len())]
                    .join(format!("r{}", rng.random_range(0..1000)));
                if std::fs::symlink_metadata(&to).is_err() {
                    let _ = std::fs::rename(from, to);
                }
            }
        });
        let _stop = StopOnDrop(stop);
        for _ in 0..4 {
            let run = a.cairn(&["sync", "--allow-mass-delete"]);
            assert!(run.ok() || run.code == Some(3), "{}", run.text());
            b.synced();
            // A rename seen halfway is never a delete, not even for a moment on another device.
            let there = token::all_in(&Picture::of(&b.root));
            let missing: Vec<_> = live.difference(&there).collect();
            assert!(missing.is_empty(), "missing on b mid-storm: {missing:?}");
        }
    });

    let p = converge(&[&a, &b]);
    let found = token::all_in(&p);
    let lost: Vec<_> = live.difference(&found).collect();
    assert!(lost.is_empty(), "lost {lost:?}");
    assert_eq!(found.len(), live.len(), "files multiplied: {:?}", p.names());
}

/// Files being written while syncs run: never a torn version on the other side.
#[test]
fn files_changing_during_sync_never_arrive_torn() {
    let sb = Sandbox::fast("churn");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    a.write("churn-big", token::make(1, 2_000_000));
    a.write("churn-small", token::make(2, 100));
    a.synced();
    b.synced();

    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let dev = a.clone();
        let stop = &stop;
        s.spawn(move || {
            let mut id = 10u64;
            while !stop.load(Ordering::SeqCst) {
                id += 1;
                // Plain non-atomic writes, as a program in the middle of saving does.
                std::fs::write(dev.path("churn-big"), token::make(id, 2_000_000)).unwrap();
                id += 1;
                std::fs::write(dev.path("churn-small"), token::make(id, 100)).unwrap();
            }
        });
        let _stop = StopOnDrop(stop);
        for _ in 0..4 {
            let run = a.cairn(&["sync"]);
            assert!(run.ok() || run.code == Some(3), "{}", run.text());
            // What reached b is always one whole version, however fast a is writing.
            let run = b.cairn(&["sync"]);
            assert!(run.ok() || run.code == Some(3), "{}", run.text());
            let _ = token::all_in(&Picture::of(&b.root));
        }
    });
    let p = converge(&[&a, &b]);
    let _ = token::all_in(&p);
}
