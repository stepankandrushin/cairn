//! A folder renamed on a server whose devices joined it by its old name: a kill before its commit, or
//! a full disk at it, renames nothing, and its chunks never move, so nothing is copied and a backup's
//! next copy links them as before. The devices go on by the old name, take the new one up at their
//! next sync and lose nothing; a new device joins the user's only folder by its new name; no other
//! folder may take the old one; and the database goes back to schema 4 only once it is renamed back.

mod common;
use common::*;

/// The folder a device's root is configured to sync.
fn configured(d: &Device) -> String {
    let c: serde_json::Value =
        serde_json::from_slice(&std::fs::read(d.path(".cairn/config.json")).unwrap()).unwrap();
    c["namespace"].as_str().unwrap().to_string()
}

#[test]
fn a_folder_renamed_under_its_devices_loses_nothing_and_they_follow_it() {
    let sb = Sandbox::on_disk("folder-rename");
    let mut server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    assert_eq!(
        configured(&a),
        "cloud",
        "the first folder, which the token alone reaches"
    );
    for i in 0..40 {
        a.write(&format!("d{}/f{i}", i % 4), format!("file {i}"));
    }
    converge(&[&a, &b]);
    let data = server.data.to_str().unwrap().to_string();
    let data_dir = server.data.clone();
    let ns_dir = |name: &str| data_dir.join("ns").join(hex::encode(name));
    let listed = |server: &Server| server.admin(&["folder", "list"]).expect_ok().stdout;
    assert!(ns_dir("cloud").exists());

    // Not under a server, which reads the former names when it starts.
    let run = server.admin(&["folder", "rename", "cloud", "photos"]);
    assert!(
        !run.ok() && run.stderr.contains("stop it first"),
        "{}",
        run.text()
    );
    server.kill();
    let rename = |env: &[(&str, &str)]| {
        run_server(
            &["folder", "rename", "cloud", "photos", "--data", &data],
            env,
        )
    };
    let unrenamed = |context: &str| {
        let list = listed(&server);
        assert!(
            list.starts_with("cloud  at ") && !list.contains("formerly"),
            "{context}: {list}"
        );
        assert!(
            ns_dir("cloud").exists() && !ns_dir("photos").exists(),
            "{context}"
        );
    };

    let run = rename(&[("CAIRN_FP_CRASH", "server.rename.commit")]);
    assert!(run.was_killed(), "never reached: {}", run.text());
    unrenamed("killed before its commit");
    let run = rename(&[("CAIRN_FP_ENOSPC", "server.rename.write")]);
    assert!(
        !run.ok() && run.stderr.contains("nothing renamed"),
        "{}",
        run.text()
    );
    unrenamed("a full disk at its commit");

    // Renamed: the chunks stay where they were, under the name it had first.
    let run = rename(&[]).expect_ok();
    assert!(
        run.stdout
            .contains(&format!("its chunks stay in {}", ns_dir("cloud").display())),
        "{}",
        run.text()
    );
    assert!(ns_dir("cloud").exists() && !ns_dir("photos").exists());
    server.restart();
    assert!(
        listed(&server).contains("photos  at ") && listed(&server).contains("formerly cloud"),
        "{}",
        listed(&server)
    );

    // The devices, configured by the old name, go on and take the new one up.
    a.write("d0/after", "after the rename");
    let run = a.synced();
    assert!(
        run.stdout
            .contains("is called \"photos\" now, and this root follows it"),
        "{}",
        run.text()
    );
    assert_eq!(configured(&a), "photos");
    converge(&[&a, &b]);
    assert_eq!(configured(&b), "photos");
    assert_eq!(b.read("d0/after"), b"after the rename");
    for i in 0..40 {
        assert_eq!(
            b.read(&format!("d{}/f{i}", i % 4)),
            format!("file {i}").as_bytes()
        );
    }

    // A new device joins the only folder by its name now, and gets all of it.
    let c = Device::new(&sb, &server, "c");
    assert_eq!(configured(&c), "photos");
    c.synced();
    assert_eq!(c.read("d0/after"), b"after the rename");
    assert_eq!(c.read("d3/f39"), b"file 39");

    // The old name is no other folder's to take.
    let run = server.admin(&["folder", "add", "cloud"]);
    assert!(
        !run.ok() && run.stderr.contains("which it still reaches"),
        "{}",
        run.text()
    );
    assert!(server.fsck().contains("clean"));
    assert!(
        !ns_dir("photos").exists(),
        "nothing ever made under the new name"
    );

    // Back to schema 4 only once its chunks are under its own name again.
    server.kill();
    let run = run_server(&["downgrade", "--data", &data], &[]);
    assert!(
        !run.ok() && run.stderr.contains("stays at schema 5") && run.stderr.contains("rename"),
        "{}",
        run.text()
    );
    // Renamed back, the devices follow it back before the former names go with the downgrade.
    run_server(
        &["folder", "rename", "photos", "cloud", "--data", &data],
        &[],
    )
    .expect_ok();
    server.restart();
    converge(&[&a, &b, &c]);
    for d in [&a, &b, &c] {
        assert_eq!(configured(d), "cloud");
    }
    server.kill();
    let run = run_server(&["downgrade", "--data", &data], &[]).expect_ok();
    assert!(run.stdout.contains("back to schema 4"), "{}", run.text());
    server.restart();
    a.write("d1/back", "at schema 5 again");
    converge(&[&a, &b, &c]);
    assert_eq!(c.read("d1/back"), b"at schema 5 again");
    assert!(server.fsck().contains("clean"));
}
