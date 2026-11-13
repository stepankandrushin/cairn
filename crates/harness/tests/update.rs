//! Updates (update.rs in the engine), with real programs in a folder of their own: a release signed with
//! a test key, and each way installing one can go wrong. What must hold whatever happens: the program
//! at its path is always a whole one, the old or the new, and runs; nothing a key this build trusts did
//! not sign is installed; and the one before is kept, to go back to.
//!
//! They run on Windows too, outside CI, but for those that need a program a shell script can play, and a
//! daemon restarting in place, which Windows leaves to the loop that runs it. On macOS, where the updater
//! takes only a program whose signature holds, the programs they make are signed ad hoc, and what a
//! script would play is compiled.

mod common;
use base64::Engine as _;
use common::*;
use ring::signature::KeyPair;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The version this workspace builds, which a release made of its own binary carries.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// What the installed program takes itself for, so that there is something to update to.
const OLD: &str = "0.0.1";

struct Key {
    pair: ring::signature::Ed25519KeyPair,
    public: String,
}

impl Key {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let der = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(der.as_ref()).unwrap();
        let public = b64().encode(pair.public_key().as_ref());
        Self { pair, public }
    }

    fn sign(&self, release: &str) -> serde_json::Value {
        let mut msg = cairn_protocol::update::SIGNED_PREFIX.to_vec();
        msg.extend_from_slice(release.as_bytes());
        serde_json::json!({
            "release": release,
            "signatures": [{"key": self.public, "signature": b64().encode(self.pair.sign(&msg).as_ref())}],
        })
    }
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// A program as an installed one: a copy of the harness's `cairn`, and for a new release one that tells
/// apart from it: the same with bytes after its end, which the loader ignores, or on macOS, where those
/// would break its signature, the same signed again under another identifier.
fn program_bytes(new: bool) -> Vec<u8> {
    let b = std::fs::read(CAIRN).unwrap();
    if !new {
        return b;
    }
    static NEW: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    NEW.get_or_init(|| newer(b)).clone()
}

#[cfg(not(target_os = "macos"))]
fn newer(mut b: Vec<u8>) -> Vec<u8> {
    b.extend_from_slice(b"\0a newer release\0");
    b
}

#[cfg(target_os = "macos")]
fn newer(b: Vec<u8>) -> Vec<u8> {
    signed_ad_hoc(&b, "cairn.newer")
}

/// A folder of its own for making a program in, as the tests run in parallel.
#[cfg(target_os = "macos")]
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cairn-update-program-{}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// This program signed ad hoc under this identifier, as the linker signs one on Apple silicon.
#[cfg(target_os = "macos")]
fn signed_ad_hoc(program: &[u8], identifier: &str) -> Vec<u8> {
    let dir = scratch();
    let path = dir.join("program");
    write_exe(&path, program);
    let out = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--identifier", identifier])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "codesign: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let signed = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    signed
}

/// A folder holding an installed `cairn`, a user dir of its own, and a release beside it.
struct Lab {
    sandbox: Sandbox,
    bin: PathBuf,
    cairn: PathBuf,
    dist: PathBuf,
    user: PathBuf,
    key: Key,
}

impl Lab {
    fn new(label: &str) -> Self {
        let sandbox = Sandbox::on_disk(label);
        let (bin, dist, user) = (
            sandbox.path("bin"),
            sandbox.path("dist"),
            sandbox.path("user"),
        );
        for d in [&bin, &dist, &user] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cairn = bin.join(exe("cairn"));
        write_exe(&cairn, &program_bytes(false));
        Self {
            sandbox,
            bin,
            cairn,
            dist,
            user,
            key: Key::new(),
        }
    }

    fn feed(&self) -> String {
        self.dist.join("update.json").to_str().unwrap().into()
    }

    /// A release of these files, `(program, name in dist, bytes)`, signed with this lab's key.
    fn publish(&self, version: &str, protocol: u32, files: &[(&str, &str, Vec<u8>)]) {
        let platform = cairn_protocol::update::platform();
        let files: Vec<_> = files
            .iter()
            .map(|(program, name, bytes)| {
                write_exe(&self.dist.join(name), bytes);
                serde_json::json!({
                    "program": program, "platform": platform, "form": "binary", "url": name,
                    "size": bytes.len(), "sha256": sha256(bytes),
                })
            })
            .collect();
        let release = serde_json::json!({
            "version": version, "protocol": protocol, "published": 1, "files": files,
        })
        .to_string();
        self.write_feed(&self.key.sign(&release));
    }

    fn write_feed(&self, signed: &serde_json::Value) {
        std::fs::write(self.feed(), signed.to_string()).unwrap();
    }

    fn publish_new(&self) {
        self.publish(
            VERSION,
            cairn_protocol::PROTOCOL_VERSION,
            &[("cairn", "cairn-new", program_bytes(true))],
        );
    }

    /// The installed `cairn`, as an old version if it is still the old program, with this lab's feed.
    fn cairn(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new(&self.cairn);
        cmd.args(args).env("CAIRN_USER_DIR", &self.user);
        if self.installed() == program_bytes(false) {
            cmd.env("CAIRN_FP_VERSION", OLD);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = spawn_retrying(&mut cmd).wait_with_output().unwrap();
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn update_now(&self, env: &[(&str, &str)]) -> Run {
        let feed = self.feed();
        self.cairn(
            &["update", "now", "--feed", &feed, "--key", &self.key.public],
            env,
        )
    }

    fn installed(&self) -> Vec<u8> {
        std::fs::read(&self.cairn).unwrap()
    }

    fn staging(&self) -> PathBuf {
        self.bin.join(".cairn-update")
    }

    fn previous(&self) -> Option<Vec<u8>> {
        std::fs::read(self.staging().join(exe("cairn-previous"))).ok()
    }

    /// Nothing half-made left beside the program.
    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(self.staging())
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".part"))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn state(&self) -> serde_json::Value {
        std::fs::read(self.user.join("update-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    /// Whatever is installed must run, and say it is `cairn`.
    #[track_caller]
    fn runs(&self) {
        let r = self.cairn(&["--version"], &[]);
        assert!(
            r.ok() && r.stdout.starts_with("cairn "),
            "the installed program does not run: {}",
            r.text()
        );
    }
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn write_exe(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Killed by a signal, or on Windows by a failpoint's TerminateProcess.
fn killed(status: std::process::ExitStatus) -> bool {
    if cfg!(windows) {
        status.code() == Some(cairn_failpoint::KILLED_EXIT_CODE)
    } else {
        status.code().is_none()
    }
}

/// A program this test process has just written may still be open in another thread's child for an
/// instant, between its fork and its exec: Linux refuses to run it meanwhile (ETXTBSY).
fn spawn_retrying(cmd: &mut Command) -> Child {
    for _ in 0..100 {
        match cmd.spawn() {
            Ok(c) => return c,
            Err(e) if cfg!(unix) && e.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("{e}"),
        }
    }
    panic!("the program stayed busy");
}

/// A program that prints this line and exits with this code: a shell script.
#[cfg(all(unix, not(target_os = "macos")))]
fn program_saying(line: &str, code: i32) -> Vec<u8> {
    format!("#!/bin/sh\necho '{line}'\nexit {code}\n").into_bytes()
}

/// A program that prints this line and exits with this code. On macOS a compiled one, signed ad hoc: the
/// updater refuses a script there, whose signature would be in xattrs that a download does not carry.
/// `cc` is there wherever Rust is, as its linker.
#[cfg(target_os = "macos")]
fn program_saying(line: &str, code: i32) -> Vec<u8> {
    let dir = scratch();
    let (source, program) = (dir.join("program.c"), dir.join("program"));
    std::fs::write(
        &source,
        format!("#include <stdio.h>\nint main(void) {{ puts({line:?}); return {code}; }}\n"),
    )
    .unwrap();
    let out = Command::new("cc")
        .arg("-o")
        .arg(&program)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cc: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&program).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    signed_ad_hoc(&bytes, "cairn.stand-in")
}

#[test]
fn a_signed_release_is_installed_and_the_one_before_kept() {
    let lab = Lab::new("update-installs");
    lab.publish_new();
    let r = lab.update_now(&[]).expect_ok();
    assert!(
        r.stdout.contains(&format!("installed {VERSION}")),
        "{}",
        r.text()
    );
    assert_eq!(lab.installed(), program_bytes(true));
    assert_eq!(lab.previous(), Some(program_bytes(false)));
    assert!(lab.leftovers().is_empty(), "{:?}", lab.leftovers());
    assert_eq!(lab.state()["installed"], VERSION);
    lab.runs();
    // The new one is that version: nothing more to do, and nothing done.
    let again = lab.update_now(&[]).expect_ok();
    assert!(again.stdout.contains("up to date"), "{}", again.text());
    assert_eq!(lab.previous(), Some(program_bytes(false)));
    let status = lab.cairn(&["update"], &[]).expect_ok();
    assert!(
        status.stdout.contains(&format!("installed: {VERSION}")),
        "{}",
        status.text()
    );
}

#[test]
fn nothing_is_installed_that_a_trusted_key_did_not_sign() {
    let lab = Lab::new("update-forged");
    lab.publish_new();
    let good: serde_json::Value =
        serde_json::from_slice(&std::fs::read(lab.feed()).unwrap()).unwrap();
    let release = good["release"].as_str().unwrap().to_string();
    let forger = Key::new();
    let cases = [
        // Signed by a key this build does not trust, under that key's name.
        forger.sign(&release),
        // That signature, claiming the trusted key's name.
        {
            let mut f = forger.sign(&release);
            f["signatures"][0]["key"] = lab.key.public.clone().into();
            f
        },
        // The release changed after it was signed: another file's hash.
        {
            let mut f = good.clone();
            f["release"] = release
                .replace(&sha256(&program_bytes(true)), &sha256(b"other"))
                .into();
            f
        },
        // No signature at all.
        serde_json::json!({ "release": release, "signatures": [] }),
    ];
    for (i, signed) in cases.iter().enumerate() {
        lab.write_feed(signed);
        let r = lab.update_now(&[]);
        assert!(!r.ok(), "case {i} was installed: {}", r.text());
        assert!(
            r.stderr.contains("not signed by a key"),
            "case {i}: {}",
            r.text()
        );
        assert_eq!(lab.installed(), program_bytes(false), "case {i}");
        assert!(lab.previous().is_none(), "case {i}");
        lab.runs();
    }
    // And the failure is kept for whoever looks.
    assert!(
        lab.state()["error"]
            .as_str()
            .unwrap_or("")
            .contains("not signed"),
        "{}",
        lab.state()
    );
}

#[test]
fn a_file_that_is_not_the_one_signed_is_not_installed() {
    let lab = Lab::new("update-bad-file");
    lab.publish_new();
    let new = program_bytes(true);
    for (what, bytes) in [
        ("cut short", new[..new.len() / 2].to_vec()),
        ("changed", {
            let mut b = new.clone();
            let last = b.len() - 1;
            b[last] ^= 1;
            b
        }),
        ("longer", {
            let mut b = new.clone();
            b.push(0);
            b
        }),
    ] {
        write_exe(&lab.dist.join("cairn-new"), &bytes);
        let r = lab.update_now(&[]);
        assert!(!r.ok(), "{what}: installed: {}", r.text());
        assert_eq!(lab.installed(), program_bytes(false), "{what}");
        assert!(lab.leftovers().is_empty(), "{what}: {:?}", lab.leftovers());
        lab.runs();
    }
}

#[test]
fn an_older_or_the_same_version_is_never_installed() {
    let lab = Lab::new("update-older");
    for version in [OLD, "0.0.0", "0.0.1-rc1"] {
        lab.publish(
            version,
            cairn_protocol::PROTOCOL_VERSION,
            &[("cairn", "cairn-new", program_bytes(true))],
        );
        let r = lab.update_now(&[]).expect_ok();
        assert!(r.stdout.contains("up to date"), "{version}: {}", r.text());
        assert_eq!(lab.installed(), program_bytes(false), "{version}");
    }
}

#[cfg(unix)]
#[test]
fn a_new_program_that_does_not_say_it_is_that_version_is_not_installed() {
    let lab = Lab::new("update-self-check");
    // Refused for its own reason, not an earlier one: on macOS a file that is no program is not signed.
    let no_program = if cfg!(target_os = "macos") {
        "not validly signed"
    } else {
        "--version: "
    };
    for (what, bytes, why) in [
        (
            "another version",
            program_saying("cairn 9.9.9", 0),
            "says it is 9.9.9",
        ),
        (
            "failing",
            program_saying(&format!("cairn {VERSION}"), 1),
            "exit status: 1",
        ),
        (
            "not cairn",
            program_saying(&format!("other {VERSION}"), 0),
            "not `cairn <version>`",
        ),
        (
            "not a program",
            b"\x7fELF but nothing more".to_vec(),
            no_program,
        ),
    ] {
        lab.publish(
            VERSION,
            cairn_protocol::PROTOCOL_VERSION,
            &[("cairn", "cairn-new", bytes)],
        );
        let r = lab.update_now(&[]);
        assert!(!r.ok(), "{what}: installed: {}", r.text());
        assert!(r.stderr.contains(why), "{what}: not {why:?}: {}", r.text());
        assert_eq!(lab.installed(), program_bytes(false), "{what}");
        assert!(lab.previous().is_none(), "{what}");
        lab.runs();
    }
}

/// On macOS the updater takes only a program whose signature holds: one changed after it was signed, or
/// one never signed, as a script would come down with its signature left behind in xattrs, is refused.
#[cfg(target_os = "macos")]
#[test]
fn a_program_whose_signature_does_not_hold_is_not_installed() {
    let lab = Lab::new("update-signature");
    let mut changed = program_bytes(false);
    changed.extend_from_slice(b"\0after its end\0");
    let script = format!("#!/bin/sh\necho cairn {VERSION}\n").into_bytes();
    for (what, bytes) in [("changed after signing", changed), ("not signed", script)] {
        lab.publish(
            VERSION,
            cairn_protocol::PROTOCOL_VERSION,
            &[("cairn", "cairn-new", bytes)],
        );
        let r = lab.update_now(&[]);
        assert!(!r.ok(), "{what}: installed: {}", r.text());
        assert!(
            r.stderr.contains("not validly signed"),
            "{what}: {}",
            r.text()
        );
        assert_eq!(lab.installed(), program_bytes(false), "{what}");
        assert!(lab.previous().is_none(), "{what}");
        lab.runs();
    }
}

#[test]
fn a_full_disk_while_downloading_installs_nothing() {
    let lab = Lab::new("update-disk-full");
    lab.publish_new();
    let r = lab.update_now(&[("CAIRN_FP_ENOSPC", "update.write")]);
    assert!(!r.ok(), "{}", r.text());
    assert!(r.stderr.contains("is full"), "{}", r.text());
    assert_eq!(lab.installed(), program_bytes(false));
    assert!(lab.leftovers().is_empty(), "{:?}", lab.leftovers());
    lab.runs();
    // Once there is room, it goes through.
    lab.update_now(&[]).expect_ok();
    assert_eq!(lab.installed(), program_bytes(true));
}

#[test]
fn a_kill_at_any_step_leaves_a_whole_program_and_the_next_run_finishes() {
    // Every step an update takes, from a run that logs them.
    let steps = {
        let lab = Lab::new("update-steps");
        lab.publish_new();
        let log = lab.sandbox.path("steps");
        lab.update_now(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
            .expect_ok();
        let mut steps: Vec<String> = Vec::new();
        for s in std::fs::read_to_string(&log).unwrap().lines() {
            if s.starts_with("update.") && !steps.iter().any(|t| t == s) {
                steps.push(s.to_string());
            }
        }
        steps
    };
    for step in [
        "update.write",
        "update.downloaded",
        "update.checked",
        "update.kept",
        "update.placed",
        "update.state",
    ] {
        assert!(
            steps.iter().any(|s| s == step),
            "{step} not among {steps:?}"
        );
    }
    for step in &steps {
        let lab = Lab::new("update-kill");
        lab.publish_new();
        let r = lab.update_now(&[("CAIRN_FP_CRASH", step)]);
        assert!(r.was_killed(), "{step}: not killed: {}", r.text());
        if cfg!(windows) && !lab.cairn.exists() {
            // Between Windows's two renames, the running one set aside and the new one not yet in its
            // place: the old one is whole as the previous, and the loop that runs the daemon puts it back.
            assert_eq!(lab.previous(), Some(program_bytes(false)), "{step}");
            std::fs::copy(lab.staging().join(exe("cairn-previous")), &lab.cairn).unwrap();
        }
        let now = lab.installed();
        assert!(
            now == program_bytes(false) || now == program_bytes(true),
            "{step}: neither the old program nor the new is in place"
        );
        lab.runs();
        let again = lab.update_now(&[]);
        assert!(again.ok(), "{step}: the next run: {}", again.text());
        assert_eq!(lab.installed(), program_bytes(true), "{step}");
        assert_eq!(lab.previous(), Some(program_bytes(false)), "{step}");
        assert!(lab.leftovers().is_empty(), "{step}: {:?}", lab.leftovers());
        lab.runs();
    }
}

/// A root of the lab's user, on a real server, so that the updater has a server to ask.
fn lab_root(lab: &Lab, server: &Server) -> PathBuf {
    let root = lab.sandbox.path("root");
    std::fs::create_dir_all(&root).unwrap();
    lab.cairn(
        &[
            "init",
            "--server",
            &server.url,
            "--token-file",
            server.token_file.to_str().unwrap(),
            "--device-name",
            "lab",
            root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    root
}

#[test]
fn a_release_on_another_protocol_waits_until_the_server_speaks_it() {
    let lab = Lab::new("update-protocol");
    let server = Server::start(lab.sandbox.path("server"));
    lab_root(&lab, &server);
    let next = cairn_protocol::PROTOCOL_VERSION + 1;
    lab.publish(
        VERSION,
        next,
        &[("cairn", "cairn-new", program_bytes(true))],
    );
    let r = lab.update_now(&[]);
    assert_eq!(r.code, Some(3), "{}", r.text());
    assert!(
        r.stdout.contains(&format!("speaks protocol {next}"))
            && r.stdout.contains(&format!(
                "speaks protocol {}",
                cairn_protocol::PROTOCOL_VERSION
            )),
        "{}",
        r.text()
    );
    assert_eq!(lab.installed(), program_bytes(false));
    // Asked to regardless, it is.
    let feed = lab.feed();
    lab.cairn(
        &[
            "update",
            "now",
            "--any-protocol",
            "--feed",
            &feed,
            "--key",
            &lab.key.public,
        ],
        &[],
    )
    .expect_ok();
    assert_eq!(lab.installed(), program_bytes(true));
}

#[cfg(unix)]
#[test]
fn the_tray_beside_it_is_updated_too() {
    let lab = Lab::new("update-tray");
    let tray = lab.bin.join("cairn-tray");
    write_exe(&tray, &program_saying(&format!("cairn-tray {OLD}"), 0));
    let new_tray = program_saying(&format!("cairn-tray {VERSION}"), 0);
    // Named as a release names them, by platform and then program: each installed by its program, under the
    // name it has here, whatever the file's.
    let platform = cairn_protocol::update::platform();
    let sync = format!("cairn-{VERSION}-{platform}-sync");
    let tray_file = format!("cairn-{VERSION}-{platform}-tray");
    lab.publish(
        VERSION,
        cairn_protocol::PROTOCOL_VERSION,
        &[
            ("cairn", &sync, program_bytes(true)),
            ("cairn-tray", &tray_file, new_tray.clone()),
        ],
    );
    let r = lab.update_now(&[]).expect_ok();
    assert!(r.stdout.contains("cairn-tray"), "{}", r.text());
    assert_eq!(std::fs::read(&tray).unwrap(), new_tray);
    assert_eq!(lab.installed(), program_bytes(true));
    assert!(lab.staging().join("cairn-tray-previous").exists());
}

#[test]
fn rollback_puts_the_one_before_back_and_it_stays_unless_asked() {
    let lab = Lab::new("update-rollback");
    lab.publish_new();
    lab.update_now(&[]).expect_ok();
    let r = lab.cairn(&["update", "rollback"], &[]).expect_ok();
    assert!(r.stdout.contains("back to"), "{}", r.text());
    assert_eq!(lab.installed(), program_bytes(false));
    assert_eq!(lab.state()["rejected"][0], VERSION);
    lab.runs();
    // The check a daemon makes does not take it again; a person asking does.
    let feed = lab.feed();
    let check = lab.cairn(
        &["update", "check", "--feed", &feed, "--key", &lab.key.public],
        &[],
    );
    assert_eq!(check.code, Some(3), "{}", check.text());
    lab.update_now(&[]).expect_ok();
    assert_eq!(lab.installed(), program_bytes(true));
    assert_eq!(lab.state()["rejected"], serde_json::json!([]));
}

struct Daemon {
    child: Child,
    root: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn start(lab: &Lab, root: &Path, env: &[(&str, &str)]) -> Self {
        let log = lab
            .sandbox
            .path(&format!("daemon-{:08x}.log", rand::random::<u32>()));
        let out = std::fs::File::create(&log).unwrap();
        let mut cmd = Command::new(&lab.cairn);
        cmd.arg("--root")
            .arg(root)
            .args(["daemon", "--retry-secs", "1"])
            .env("CAIRN_USER_DIR", &lab.user)
            .env("CAIRN_FP_TUNE", "update.tick_ms=100,client.retry_ms=1");
        if lab.installed() == program_bytes(false) {
            cmd.env("CAIRN_FP_VERSION", OLD);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out);
        Self {
            child: spawn_retrying(&mut cmd),
            root: root.to_path_buf(),
            log,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn status(&self) -> serde_json::Value {
        std::fs::read(self.root.join(".cairn/daemon.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[track_caller]
fn wait_until(what: &str, d: &Daemon, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{what}: not within 60 s; the daemon's log:\n{}",
            d.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn a_daemon_restarts_into_the_new_version_in_the_same_process() {
    let lab = Lab::new("update-daemon-restart");
    let server = Server::start(lab.sandbox.path("server"));
    let root = lab_root(&lab, &server);
    let d = Daemon::start(&lab, &root, &[]);
    wait_until("the daemon's first sync", &d, || {
        d.status()["syncs"].as_u64() >= Some(1)
    });
    assert_eq!(d.status()["version"], OLD);
    let pid = d.status()["pid"].clone();
    #[cfg(target_os = "linux")]
    let watching = {
        wait_until("synced", &d, || d.status()["state"] == "synced");
        inotify_descriptors(pid.as_u64().unwrap())
    };
    lab.publish_new();
    let r = lab.update_now(&[]).expect_ok();
    assert!(
        r.stdout.contains(&format!("restarted on {VERSION}")),
        "{}",
        r.text()
    );
    wait_until("a sync on the new version", &d, || {
        d.status()["version"] == VERSION && d.status()["state"] == "synced"
    });
    assert_eq!(d.status()["pid"], pid, "not restarted in place");
    assert!(
        d.log().contains("restarting into the new one"),
        "{}",
        d.log()
    );
    // The old version's watches went with it: one carried over holds a whole tree's watches against
    // the kernel's limit, another tree's worth at each update, until that limit stops the watching.
    #[cfg(target_os = "linux")]
    {
        assert!(watching > 0, "the daemon watched nothing");
        assert_eq!(
            inotify_descriptors(pid.as_u64().unwrap()),
            watching,
            "descriptors of inotify kept through the restart"
        );
    }
}

/// How many of a process's descriptors are inotify's.
#[cfg(target_os = "linux")]
fn inotify_descriptors(pid: u64) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| target.as_os_str() == "anon_inode:inotify")
        .count()
}

#[cfg(unix)]
#[test]
fn a_daemon_updates_itself_from_the_feed_unless_set_to_notify() {
    let lab = Lab::new("update-daemon-auto");
    let server = Server::start(lab.sandbox.path("server"));
    let root = lab_root(&lab, &server);
    lab.publish_new();
    let settings =
        serde_json::json!({ "policy": "notify", "feed": lab.feed(), "keys": [lab.key.public] });
    std::fs::write(lab.user.join("update-settings.json"), settings.to_string()).unwrap();
    let tune = [(
        "CAIRN_FP_TUNE",
        "update.tick_ms=100,update.first_secs=0,client.retry_ms=1",
    )];
    {
        let d = Daemon::start(&lab, &root, &tune);
        wait_until("the update shown as waiting", &d, || {
            d.status()["update"]["available"] == VERSION
        });
        assert!(
            d.status()["update"]["waits"]
                .as_str()
                .unwrap_or("")
                .contains("set to notify"),
            "{}",
            d.status()
        );
        assert_eq!(lab.installed(), program_bytes(false));
    }
    let settings =
        serde_json::json!({ "policy": "auto", "feed": lab.feed(), "keys": [lab.key.public] });
    std::fs::write(lab.user.join("update-settings.json"), settings.to_string()).unwrap();
    let d = Daemon::start(&lab, &root, &tune);
    wait_until("the daemon on the new version", &d, || {
        d.status()["version"] == VERSION
    });
    assert_eq!(lab.installed(), program_bytes(true));
}

#[test]
fn a_version_that_keeps_killing_the_daemon_is_taken_back() {
    let lab = Lab::new("update-probation");
    let server = Server::start(lab.sandbox.path("server"));
    let root = lab_root(&lab, &server);
    lab.publish_new();
    lab.update_now(&[]).expect_ok();
    assert_eq!(lab.installed(), program_bytes(true));
    // Killed in its first sync, each time it starts.
    for start in 1..=3 {
        let mut d = Daemon::start(&lab, &root, &[("CAIRN_FP_CRASH", "daemon.sync")]);
        let status = d.child.wait().unwrap();
        assert!(killed(status), "start {start}: {status}");
        assert_eq!(lab.installed(), program_bytes(true), "start {start}");
    }
    let mut d = Daemon::start(&lab, &root, &[("CAIRN_FP_CRASH", "daemon.sync")]);
    // It takes the version back, starts the one before, and that one is killed in its sync too.
    let _ = d.child.wait();
    assert_eq!(lab.installed(), program_bytes(false));
    assert_eq!(lab.state()["rejected"][0], VERSION);
    lab.runs();
    // A daemon that gets through a sync on a new version keeps it.
    lab.update_now(&[]).expect_ok();
    let d = Daemon::start(&lab, &root, &[]);
    wait_until("a sync", &d, || d.status()["state"] == "synced");
    drop(d);
    assert!(lab.state()["probation"].is_null(), "{}", lab.state());
    assert_eq!(lab.installed(), program_bytes(true));
}

/// The files of `dist` over HTTP, `/latest/<name>` redirecting to `/v/<name>` as a GitHub release's
/// latest download does. Its URL.
fn serve(dist: PathBuf) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { continue };
            let dist = dist.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let text = String::from_utf8_lossy(&head);
                let path = text.split(' ').nth(1).unwrap_or("/").to_string();
                let reply = if let Some(name) = path.strip_prefix("/latest/") {
                    format!("HTTP/1.1 302 Found\r\nLocation: /v/{name}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
                } else if let Some(body) = path
                    .strip_prefix("/v/")
                    .and_then(|name| std::fs::read(dist.join(name)).ok())
                {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(&body);
                    r
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = c.write_all(&reply);
            });
        }
    });
    url
}

#[test]
fn a_release_comes_over_http_through_a_redirect() {
    let lab = Lab::new("update-http");
    lab.publish_new();
    let feed = format!("{}/latest/update.json", serve(lab.dist.clone()));
    let r = lab
        .cairn(
            &["update", "now", "--feed", &feed, "--key", &lab.key.public],
            &[],
        )
        .expect_ok();
    assert!(
        r.stdout.contains(&format!("installed {VERSION}")),
        "{}",
        r.text()
    );
    assert_eq!(lab.installed(), program_bytes(true));
    // A feed that is not there is an error said, not an update.
    let gone = format!("{}/v/nothing.json", serve(lab.dist.clone()));
    let r = lab.cairn(
        &["update", "check", "--feed", &gone, "--key", &lab.key.public],
        &[],
    );
    assert!(!r.ok() && r.stderr.contains("answered 404"), "{}", r.text());
}

/// An address nothing listens on, as a site that is down.
fn closed_feed() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}/update.json", listener.local_addr().unwrap())
}

#[test]
fn a_feed_that_does_not_answer_is_passed_over_for_the_next_and_said() {
    // The first is down: the release comes from the second, and what the first did is kept and shown.
    let lab = Lab::new("update-fallback");
    lab.publish_new();
    let closed = closed_feed();
    let feeds = format!("{closed},{}", lab.feed());
    let r = lab
        .cairn(
            &["update", "now", "--feed", &feeds, "--key", &lab.key.public],
            &[],
        )
        .expect_ok();
    assert!(
        r.stdout.contains(&format!("installed {VERSION}"))
            && r.stdout.contains(&format!("{closed}: cannot be reached")),
        "{}",
        r.text()
    );
    assert_eq!(lab.installed(), program_bytes(true));
    let state = lab.state();
    assert_eq!(state["answered"], lab.feed(), "{state}");
    assert!(
        state["unanswered"]
            .as_str()
            .is_some_and(|u| u.contains(&closed)),
        "{state}"
    );
    let status = lab.cairn(&["update"], &[]).expect_ok();
    assert!(
        status.stdout.contains("not answered then:") && status.stdout.contains(&closed),
        "{}",
        status.text()
    );

    // The first answers with a release no trusted key signed, a file of its own beside it: the
    // second's release is installed, with the second's file, never the first's.
    let lab = Lab::new("update-fallback-forged");
    lab.publish_new();
    let mirror = lab.sandbox.path("mirror");
    std::fs::create_dir_all(&mirror).unwrap();
    let good: serde_json::Value =
        serde_json::from_slice(&std::fs::read(lab.feed()).unwrap()).unwrap();
    let forged = Key::new().sign(good["release"].as_str().unwrap());
    let forged_feed = mirror.join("update.json");
    std::fs::write(&forged_feed, forged.to_string()).unwrap();
    std::fs::write(mirror.join("cairn-new"), b"not the release's program").unwrap();
    let forged_feed = forged_feed.to_str().unwrap().to_string();
    let feeds = format!("{forged_feed},{}", lab.feed());
    let r = lab
        .cairn(
            &["update", "now", "--feed", &feeds, "--key", &lab.key.public],
            &[],
        )
        .expect_ok();
    assert!(
        r.stdout.contains(&format!("installed {VERSION}")),
        "{}",
        r.text()
    );
    assert_eq!(lab.installed(), program_bytes(true));
    assert!(
        lab.state()["unanswered"]
            .as_str()
            .is_some_and(|u| u.contains(&forged_feed) && u.contains("not signed by a key")),
        "{}",
        lab.state()
    );

    // Neither answers: nothing is installed, and the error says what each did.
    let lab = Lab::new("update-fallback-none");
    let closed = closed_feed();
    let feeds = format!("{closed},{forged_feed}");
    let r = lab.cairn(
        &["update", "now", "--feed", &feeds, "--key", &lab.key.public],
        &[],
    );
    assert!(!r.ok(), "{}", r.text());
    assert!(
        r.stderr.contains(&closed) && r.stderr.contains("not signed by a key"),
        "{}",
        r.text()
    );
    assert_eq!(lab.installed(), program_bytes(false));
    assert!(lab.previous().is_none());
    let error = lab.state()["error"].as_str().unwrap_or("").to_string();
    assert!(
        error.contains(&closed) && error.contains(&forged_feed),
        "{error}"
    );
    lab.runs();
}

#[test]
fn a_daemon_says_a_feed_passed_over_once_and_again_when_it_answers() {
    let lab = Lab::new("update-daemon-fallback");
    let server = Server::start(lab.sandbox.path("server"));
    let root = lab_root(&lab, &server);
    lab.publish_new();
    let mirror = lab.sandbox.path("mirror");
    std::fs::create_dir_all(&mirror).unwrap();
    let first = mirror.join("update.json");
    let feeds = format!("{},{}", first.to_str().unwrap(), lab.feed());
    // Notify, so the release waits and every check asks the feeds again.
    let settings =
        serde_json::json!({ "policy": "notify", "feed": feeds, "keys": [lab.key.public] });
    std::fs::write(lab.user.join("update-settings.json"), settings.to_string()).unwrap();
    let tune = [(
        "CAIRN_FP_TUNE",
        "update.tick_ms=100,update.first_secs=0,update.every_secs=1,client.retry_ms=1",
    )];
    let d = Daemon::start(&lab, &root, &tune);
    let checked = || lab.state()["checked"].as_i64().unwrap_or(0);
    let said = |what: &str| d.log().matches(what).count();
    wait_until("the second feed's release shown", &d, || {
        d.status()["update"]["available"] == VERSION
    });
    let since = checked();
    wait_until("three more checks", &d, || checked() >= since + 3);
    assert_eq!(said("answered instead"), 1, "{}", d.log());
    assert_eq!(lab.state()["answered"], lab.feed(), "{}", lab.state());
    // The first answers again: said once, and the status no longer holds what it said.
    std::fs::copy(lab.feed(), &first).unwrap();
    wait_until("the first feed answering", &d, || {
        lab.state()["answered"] == first.to_str().unwrap()
    });
    let since = checked();
    wait_until("three more checks", &d, || checked() >= since + 3);
    assert_eq!(said("answers again"), 1, "{}", d.log());
    assert_eq!(said("answered instead"), 1, "{}", d.log());
    assert!(lab.state()["unanswered"].is_null(), "{}", lab.state());
    assert_eq!(lab.installed(), program_bytes(false));
}
