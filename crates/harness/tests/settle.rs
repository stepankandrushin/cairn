//! A slow device beside a busy one: a sync whose every round takes longer than another device takes to
//! commit still comes to agree. A round that applied something is confirmed by a round against the same
//! sequence, which takes no new commits, so the sync agrees as of that sequence and leaves what came
//! after it to the next; and nothing a person changes here in that round is taken for agreed.

mod common;
use common::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A device that rewrites one file and syncs it, one sync after another, until told to stop.
struct Busy {
    stop: Arc<AtomicBool>,
    commits: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Busy {
    fn start(d: &Device, every: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let commits = Arc::new(AtomicU64::new(0));
        let (d, s, c) = (d.clone(), stop.clone(), commits.clone());
        let thread = std::thread::spawn(move || {
            let mut i = 0u64;
            while !s.load(Ordering::SeqCst) {
                i += 1;
                d.write("hot", format!("rewritten {i}"));
                if d.sync().ok() {
                    c.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(every);
            }
        });
        Self {
            stop,
            commits,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.stop();
    }
}

fn steps_of(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The note's case: a joins with a tree, b joins with a copy of it while c rewrites and syncs one file
/// every 200 ms or so, and every round of b's takes longer than that. Before, b ran 12 rounds and gave up,
/// "the tree kept changing", having agreed on nothing, so its next sync looked at the whole tree again.
/// Now b agrees within a few rounds, and its next sync takes what changed alone.
#[test]
fn a_join_slower_than_a_busy_device_still_agrees() {
    let sb = Sandbox::on_disk("settle-join");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for d in 0..30 {
        for f in 0..100 {
            let (rel, data) = (format!("d{d}/f{f}"), format!("{d} {f}"));
            a.write(&rel, &data);
            b.write(&rel, &data);
        }
    }
    a.write("hot", "first");
    a.synced();
    // The busy device holds the hot file alone, so that its syncs are quick in a harness build too.
    let c = Device::new(&sb, &server, "c");
    std::fs::write(c.path(".cairn/ignore"), "/d*/\n").unwrap();
    c.synced();
    assert_eq!(c.read("hot"), b"first");

    let mut busy = Busy::start(&c, Duration::from_millis(200));
    // Each of b's rounds is longer than c's commit interval.
    let start = std::time::Instant::now();
    while busy.commits.load(Ordering::SeqCst) < 3 {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the busy device does not commit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let interval = start.elapsed() / 3;
    let steps = sb.path("b-steps");
    let run = b.sync_env(&[
        ("CAIRN_FP_DELAY", "sync.scanned:1000"),
        ("CAIRN_FP_LOG", steps.to_str().unwrap()),
    ]);
    let commits_during = busy.commits.load(Ordering::SeqCst);
    eprintln!("the busy device committed about every {interval:?}");
    let rounds = steps_of(&steps)
        .iter()
        .filter(|s| *s == "sync.round")
        .count();
    assert!(
        run.ok(),
        "b's join did not agree beside a busy device ({commits_during} commits by c meanwhile, {rounds} rounds): {}",
        run.text()
    );
    assert!(!run.text().contains("kept changing"), "{}", run.text());
    assert!(
        rounds < 12,
        "{rounds} rounds, with c committing {commits_during} times"
    );
    assert!(
        steps_of(&steps).iter().any(|s| s == "sync.round.pinned"),
        "no round confirmed against the same sequence: {}",
        run.text()
    );

    // The next sync, with c still busy, takes what changed alone, and agrees too.
    let run = b.sync_env(&[("CAIRN_FP_DELAY", "sync.scanned:1000")]);
    assert!(run.ok(), "{}", run.text());
    assert!(
        run.stdout
            .contains("entries: the rest are as the last agreement left them"),
        "the next sync did not take what changed alone: {}",
        run.text()
    );
    busy.stop();

    // Nothing lost anywhere: once c rests, all three agree.
    assert!(busy.commits.load(Ordering::SeqCst) > commits_during);
    converge(&[&a, &b]);
    c.synced();
    assert_eq!(c.read("hot"), a.read("hot"));
}

/// What a person changes here in the round that confirms an apply, after its scan, is not taken for
/// agreed: the round against the same sequence ends the sync, and the next sends the change, whether the
/// sync takes the whole tree or what changed alone.
#[test]
fn a_change_here_in_the_round_that_confirms_an_apply_is_sent_by_the_next_sync() {
    for incremental in [true, false] {
        let label = format!("settle-confirm-{incremental}");
        let sb = Sandbox::on_disk(&label);
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        a.write("from-a", "1");
        a.write("kept/old", "old");
        a.synced();
        b.synced();
        b.synced();
        a.write("from-a", "2");
        a.synced();

        let gate = sb.path("gate");
        std::fs::create_dir(&gate).unwrap();
        let spec = format!("sync.scanned#2:{}", gate.display());
        let steps = sb.path("b-steps");
        let mut env = vec![
            ("CAIRN_FP_PAUSE", spec.as_str()),
            ("CAIRN_FP_LOG", steps.to_str().unwrap()),
        ];
        if !incremental {
            env.push(("CAIRN_FP_INCR", "0"));
        }
        let b2 = b.clone();
        let env_owned: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let syncing = std::thread::spawn(move || {
            let env: Vec<(&str, &str)> = env_owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            b2.sync_env(&env)
        });
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "never paused");
            std::thread::sleep(Duration::from_millis(10));
        }
        // After the scan of the round that confirms the apply: an edit of what arrived, an edit of what
        // did not, and a new file.
        assert_eq!(b.read("from-a"), b"2");
        b.write("from-a", "3, written here as the apply was confirmed");
        b.write("kept/old", "edited here as the apply was confirmed");
        b.write("kept/new", "made here as the apply was confirmed");
        std::fs::write(gate.join("go"), "").unwrap();
        let run = syncing.join().unwrap();
        assert!(run.ok(), "{}", run.text());
        let steps = steps_of(&steps);
        assert!(
            steps.iter().any(|s| s == "sync.round.pinned"),
            "the second round was not one against the same sequence: {}",
            run.text()
        );
        assert!(steps.iter().any(|s| s == "sync.finalized"));

        // The next sync sends all three, and a has them.
        let run = b.synced();
        assert!(!run.stdout.contains("0 commits pushed"), "{}", run.text());
        a.synced();
        assert_eq!(
            a.read("from-a"),
            b"3, written here as the apply was confirmed"
        );
        assert_eq!(
            a.read("kept/old"),
            b"edited here as the apply was confirmed"
        );
        assert_eq!(a.read("kept/new"), b"made here as the apply was confirmed");
        converge(&[&a, &b]);
    }
}
