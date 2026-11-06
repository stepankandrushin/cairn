//! The network failing a request in a way that says nothing about the request: the kernel out of
//! socket buffers (ENOBUFS), a connection reset as the answer comes back. The client tries the request
//! again, each time in its output, and past its budget the sync fails as it did without retries, with
//! nothing lost: a commit stays queued, and the next sync delivers it.

mod common;
use common::*;

/// ENOBUFS, as each platform numbers it.
const ENOBUFS: i32 = if cfg!(target_os = "macos") {
    55
} else if cfg!(windows) {
    10055
} else {
    105
};
/// ECONNRESET.
const ECONNRESET: i32 = if cfg!(target_os = "macos") {
    54
} else if cfg!(windows) {
    10054
} else {
    104
};
/// The tries after the first that the client allows itself.
const RETRIES: u32 = 7;

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// The server's journal: each entry's operation ID, in order.
fn journal(server: &Server) -> Vec<Vec<u8>> {
    let conn = rusqlite::Connection::open(server.data.join("meta.sqlite")).unwrap();
    let mut stmt = conn
        .prepare("SELECT op_id FROM journal ORDER BY seq")
        .unwrap();
    let ids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    ids
}

/// The operation IDs of the commits this device has queued and not yet had an answer for.
fn outbox(d: &Device) -> Vec<Vec<u8>> {
    let conn = rusqlite::Connection::open(d.path(".cairn/state.sqlite")).unwrap();
    let mut stmt = conn.prepare("SELECT op_id FROM outbox").unwrap();
    let ids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    ids
}

/// The next `n` requests at `point` fail with `errno`, before they go out or, `lost`, with the answer.
fn failing(point: &str, errno: i32, n: u32, lost: bool) -> String {
    format!("{point}:{errno}:{n}{}", if lost { ":lost" } else { "" })
}

/// Each new try the run said, in order.
fn retries(run: &Run) -> Vec<String> {
    run.stderr
        .lines()
        .filter(|l| l.contains("; trying again in "))
        .map(str::to_string)
        .collect()
}

/// Said once for each try after the first, numbered, naming the error and the request.
#[track_caller]
fn said_each(run: &Run, n: u32, errno: i32, what: &str) {
    said_each_of(run, n, RETRIES, &format!("(os error {errno})"), what);
}

/// The same, out of a budget of `of`, each line saying `error`.
#[track_caller]
fn said_each_of(run: &Run, n: u32, of: u32, error: &str, what: &str) {
    let said = retries(run);
    assert_eq!(said.len(), n as usize, "{}", run.text());
    for (i, line) in said.iter().enumerate() {
        assert!(
            line.starts_with(what)
                && line.contains(error)
                && line.ends_with(&format!(", {} of {of}", i + 1)),
            "{line}\n{}",
            run.text()
        );
    }
}

/// Up to the last try the client allows itself, at each request a sync of a new file makes: it ends
/// synced, with one journal entry for its one commit, and each new try said.
#[test]
fn a_request_the_network_fails_within_the_budget_is_tried_again() {
    let (_sb, server, a, b) = pair("net-within");
    a.synced();
    for (i, (point, what)) in [
        ("client.info", "server info"),
        ("client.changes", "reading"),
        ("client.missing", "missing chunks"),
        ("client.upload", "uploading chunks"),
        ("client.commit", "committing"),
    ]
    .into_iter()
    .enumerate()
    {
        let before = journal(&server);
        a.write(&format!("f{i}"), format!("content {i} for {point}"));
        let spec = failing(point, ENOBUFS, RETRIES, false);
        let run = a.sync_env(&[("CAIRN_FP_NET", &spec)]);
        assert!(run.ok(), "{point}: {}", run.text());
        said_each(&run, RETRIES, ENOBUFS, what);
        assert!(
            run.stdout.contains(&format!(
                "tried requests again after the network failed them: {RETRIES} time(s)"
            )),
            "{point}: {}",
            run.text()
        );
        assert!(!run.stdout.contains("refused a commit"), "{}", run.text());
        let after = journal(&server);
        assert_eq!(after.len(), before.len() + 1, "{point}: {}", run.text());
        assert!(outbox(&a).is_empty(), "{point}");
    }
    // Every operation ID once.
    let ids = journal(&server);
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());

    // And on the way down.
    let spec = failing("client.download", ENOBUFS, RETRIES, false);
    let run = b.sync_env(&[("CAIRN_FP_NET", &spec)]);
    assert!(run.ok(), "{}", run.text());
    said_each(&run, RETRIES, ENOBUFS, "downloading chunks");
    for i in 0..5 {
        assert_eq!(a.read(&format!("f{i}")), b.read(&format!("f{i}")));
    }
    converge(&[&a, &b]);
}

/// One failure more than the budget: the sync fails naming the error, as it did without retries; the
/// commit stays queued, and the next sync delivers it, its chunks all there.
#[test]
fn a_request_the_network_fails_past_the_budget_fails_the_sync_and_loses_nothing() {
    let (_sb, mut server, a, b) = pair("net-past");
    a.synced();
    for (i, (point, what)) in [
        ("client.upload", "uploading chunks"),
        ("client.commit", "committing"),
    ]
    .into_iter()
    .enumerate()
    {
        let before = journal(&server);
        let name = format!("f{i}");
        a.write(&name, format!("content {i}, which {point} fails"));
        let spec = failing(point, ENOBUFS, RETRIES + 1, false);
        let run = a.sync_env(&[("CAIRN_FP_NET", &spec)]);
        assert!(!run.ok() && !run.was_killed(), "{point}: {}", run.text());
        said_each(&run, RETRIES, ENOBUFS, what);
        let error = run.stderr.lines().last().unwrap_or_default();
        assert!(
            error.contains(&format!("(os error {ENOBUFS})"))
                && error.contains(&format!("tried {} times", RETRIES + 1)),
            "{point}: {}",
            run.text()
        );
        assert_eq!(journal(&server), before, "{point}");
        let queued = outbox(&a);
        if point == "client.commit" {
            assert_eq!(queued.len(), 1, "{point}: the commit is to stay queued");
        } else {
            assert!(queued.is_empty(), "{point}: no commit before its chunks");
        }

        let run = a.synced();
        assert!(retries(&run).is_empty(), "{}", run.text());
        let after = journal(&server);
        assert_eq!(after.len(), before.len() + 1, "{point}: {}", run.text());
        if let Some(id) = queued.first() {
            assert_eq!(after.last(), Some(id), "the queued commit, not another");
        }
        assert!(outbox(&a).is_empty());
        b.synced();
        assert_eq!(b.read(&name), a.read(&name), "{point}");
    }
    server.fsck();
}

/// The server took the commit and its answer was lost: the next try gets the first answer, so the
/// journal has the commit once and the sync ends synced. And with every answer lost, past the budget,
/// the next sync's replay of the queue gets it.
#[test]
fn a_commit_whose_answer_is_lost_gets_the_first_answer() {
    let (_sb, mut server, a, b) = pair("net-lost");
    a.synced();

    a.write("f0", "the answer to its upload is lost");
    a.write("f1", "and to its commit");
    let before = journal(&server);
    let spec = failing("client.upload", ECONNRESET, 1, true);
    let run = a.sync_env(&[("CAIRN_FP_NET", &spec)]);
    assert!(run.ok(), "{}", run.text());
    said_each(&run, 1, ECONNRESET, "uploading chunks");
    assert_eq!(journal(&server).len(), before.len() + 1);

    a.write("f2", "its commit's answer is lost");
    let before = journal(&server);
    let spec = failing("client.commit", ECONNRESET, 1, true);
    let run = a.sync_env(&[("CAIRN_FP_NET", &spec)]);
    assert!(run.ok(), "{}", run.text());
    said_each(&run, 1, ECONNRESET, "committing");
    assert!(!run.stdout.contains("refused a commit"), "{}", run.text());
    assert!(run.stdout.contains("1 commits pushed"), "{}", run.text());
    assert_eq!(journal(&server).len(), before.len() + 1, "{}", run.text());
    assert!(outbox(&a).is_empty());
    // Nothing left to send: the server's answer was taken as the first one.
    let run = a.synced();
    assert!(run.stdout.contains("0 commits pushed"), "{}", run.text());

    a.write("f3", "every answer to its commit is lost");
    let before = journal(&server);
    let spec = failing("client.commit", ECONNRESET, RETRIES + 1, true);
    let run = a.sync_env(&[("CAIRN_FP_NET", &spec)]);
    assert!(!run.ok() && !run.was_killed(), "{}", run.text());
    said_each(&run, RETRIES, ECONNRESET, "committing");
    // Taken by the first try, and by none of the others.
    let after = journal(&server);
    assert_eq!(after.len(), before.len() + 1, "{}", run.text());
    let queued = outbox(&a);
    assert_eq!(queued, vec![after.last().unwrap().clone()]);
    let run = a.synced();
    assert!(!run.stdout.contains("refused a commit"), "{}", run.text());
    assert_eq!(journal(&server), after, "the replay is the same commit");
    assert!(outbox(&a).is_empty());

    converge(&[&a, &b]);
    server.fsck();
}

/// A refusal is about the request, and is not tried again: here a token the server does not take.
#[test]
fn a_refused_request_is_not_tried_again() {
    let (sb, server, a, _b) = pair("net-refused");
    a.synced();
    let config = a.path(".cairn/config.json");
    let text = std::fs::read_to_string(&config).unwrap();
    let token = std::fs::read_to_string(&server.token_file).unwrap();
    std::fs::write(&config, text.replace(token.trim(), "not-the-token")).unwrap();
    let run = a.sync();
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("refused the access token"),
        "{}",
        run.text()
    );
    assert!(retries(&run).is_empty(), "{}", run.text());
    drop(sb);
}

/// A command a person waits on tries three times more, over a few seconds, not a minute: a mistyped
/// address fails at once, still saying each try.
#[test]
fn a_command_tries_again_for_a_few_seconds() {
    let (sb, server, a, _b) = pair("net-short");
    a.synced();
    let spec = failing("client.info", ENOBUFS, 3, false);
    let run = a.cairn_env(&["log"], &[("CAIRN_FP_NET", &spec)]);
    assert!(run.ok(), "{}", run.text());
    said_each_of(&run, 3, 3, &format!("(os error {ENOBUFS})"), "server info");
    assert!(
        run.stderr.contains("server info: answered on try 4 after "),
        "{}",
        run.text()
    );
    let spec = failing("client.info", ENOBUFS, 4, false);
    let run = a.cairn_env(&["log"], &[("CAIRN_FP_NET", &spec)]);
    assert!(!run.ok(), "{}", run.text());
    said_each_of(&run, 3, 3, &format!("(os error {ENOBUFS})"), "server info");
    assert!(run.stderr.contains("; tried 4 times"), "{}", run.text());

    // A name that does not exist, as a mistyped one: looked up again, and refused as a root.
    let r = sb.path("r");
    std::fs::create_dir_all(&r).unwrap();
    let token = server.token_file.to_str().unwrap().to_string();
    let run = run_init(&r, "http://cairn-test.invalid:1", &token);
    assert!(!run.ok(), "{}", run.text());
    said_each_of(
        &run,
        3,
        3,
        "cannot look up the server's name",
        "server info",
    );
    assert!(
        run.stderr.contains("not made a sync root"),
        "{}",
        run.text()
    );
}

fn run_init(root: &std::path::Path, url: &str, token: &str) -> Run {
    run(
        &[
            "init",
            "--server",
            url,
            "--allow-http",
            "--token-file",
            token,
            "--device-name",
            "r",
            root.to_str().unwrap(),
        ],
        &[],
    )
}

/// The server killed and started again after it took a commit, before it answered, as a restart
/// does: the client finds the connection closed and then refused, tries again, and the server's
/// answer to the same operation ID is the first one.
#[test]
fn a_server_restarted_before_it_answers_is_asked_again() {
    let (sb, mut server, a, b) = pair("net-restart");
    a.synced();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    server.set_env(vec![(
        "CAIRN_FP_PAUSE".into(),
        format!("server.commit.before_ack:{}", gate.display()),
    )]);
    server.restart();
    a.write("f", "committed before the restart, answered after it");
    let before = journal(&server);
    // Waits of 0.25 s and up, about 30 s in all: time for the restart.
    let syncing = {
        let a = a.clone();
        std::thread::spawn(move || a.sync_env(&[("CAIRN_FP_TUNE", "client.retry_ms=250")]))
    };
    let start = std::time::Instant::now();
    while !gate.join("reached").exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(60),
            "the commit never came"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    server.kill();
    server.set_env(Vec::new());
    server.restart();
    let run = syncing.join().unwrap();
    assert!(run.ok(), "{}", run.text());
    let said = retries(&run);
    assert!(
        !said.is_empty() && said[0].starts_with("committing: "),
        "{}",
        run.text()
    );
    assert!(
        run.stderr.contains("committing: answered on try "),
        "{}",
        run.text()
    );
    assert!(!run.stdout.contains("refused a commit"), "{}", run.text());
    assert!(run.stdout.contains("1 commits pushed"), "{}", run.text());
    assert_eq!(journal(&server).len(), before.len() + 1, "{}", run.text());
    assert!(outbox(&a).is_empty());
    b.synced();
    assert_eq!(b.read("f"), a.read("f"));
    server.fsck();
}
