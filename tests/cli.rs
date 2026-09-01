use std::{fs, process::Command};

fn arbora(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    arbora_with_cache(project, &project.join(".test-cache"), args)
}

fn arbora_with_cache(
    project: &std::path::Path,
    cache: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_arbora"))
        .env("ARBORA_CACHE_DIR", cache)
        .arg("--project")
        .arg(project)
        .args(args)
        .output()
        .unwrap()
}

fn succeeds(output: &std::process::Output) -> bool {
    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }
    output.status.success()
}

fn locked_root(project: &std::path::Path) -> String {
    let lock = fs::read_to_string(project.join("assets.lock")).unwrap();
    lock.lines()
        .find_map(|line| line.strip_prefix("root = \""))
        .and_then(|line| line.strip_suffix('"'))
        .unwrap()
        .to_owned()
}

#[test]
fn push_diff_pull_and_verify_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    assert!(succeeds(&arbora(project, &["init"])));
    fs::create_dir(project.join("assets/textures")).unwrap();
    fs::write(project.join("assets/readme.txt"), b"version one\n").unwrap();
    fs::write(project.join("assets/textures/a.bin"), [1, 2, 3]).unwrap();
    assert!(succeeds(&arbora(project, &["push"])));
    assert!(arbora(project, &["status"]).status.success());

    fs::write(project.join("assets/readme.txt"), b"changed\n").unwrap();
    fs::write(project.join("assets/new.txt"), b"new\n").unwrap();
    let diff = arbora(project, &["diff"]);
    assert!(diff.status.success());
    let stdout = String::from_utf8(diff.stdout).unwrap();
    assert!(stdout.contains("M readme.txt"));
    assert!(stdout.contains("A new.txt"));

    assert!(arbora(project, &["pull"]).status.success());
    assert_eq!(
        fs::read(project.join("assets/readme.txt")).unwrap(),
        b"version one\n"
    );
    assert!(!project.join("assets/new.txt").exists());
    assert!(arbora(project, &["verify"]).status.success());
    assert!(arbora(project, &["gc"]).status.success());
}

#[test]
fn ignore_patterns_affect_scans_and_survive_pull() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    assert!(succeeds(&arbora(project, &["init"])));
    fs::write(
        project.join(".aboraignore"),
        "# generated files\n*.tmp\ncache/\n!keep.tmp\n",
    )
    .unwrap();
    fs::create_dir(project.join("assets/cache")).unwrap();
    fs::write(project.join("assets/tracked.txt"), b"tracked\n").unwrap();
    fs::write(project.join("assets/scratch.tmp"), b"one\n").unwrap();
    fs::write(project.join("assets/keep.tmp"), b"kept by negation\n").unwrap();
    fs::write(project.join("assets/cache/data.bin"), b"cache\n").unwrap();
    assert!(succeeds(&arbora(project, &["push"])));

    fs::write(project.join("assets/scratch.tmp"), b"two\n").unwrap();
    fs::write(project.join("assets/cache/data.bin"), b"changed\n").unwrap();
    let status = arbora(project, &["status"]);
    assert!(succeeds(&status));
    assert!(
        String::from_utf8(status.stdout)
            .unwrap()
            .starts_with("clean\n")
    );

    fs::write(project.join("assets/stale.txt"), b"remove me\n").unwrap();
    fs::create_dir(project.join("assets/stale-dir")).unwrap();
    fs::write(
        project.join("assets/stale-dir/file.txt"),
        b"remove me too\n",
    )
    .unwrap();
    assert!(succeeds(&arbora(project, &["pull"])));
    assert_eq!(
        fs::read(project.join("assets/scratch.tmp")).unwrap(),
        b"two\n"
    );
    assert_eq!(
        fs::read(project.join("assets/cache/data.bin")).unwrap(),
        b"changed\n"
    );
    assert!(!project.join("assets/stale.txt").exists());
    assert!(!project.join("assets/stale-dir").exists());
    assert!(project.join("assets/keep.tmp").exists());
}

#[test]
fn gc_keeps_objects_referenced_by_another_project() {
    let temp = tempfile::tempdir().unwrap();
    let project_a = temp.path().join("a");
    let project_b = temp.path().join("b");
    let shared_cache = temp.path().join("shared-cache");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();

    for project in [&project_a, &project_b] {
        assert!(succeeds(&arbora_with_cache(
            project,
            &shared_cache,
            &["init"]
        )));
    }
    fs::write(project_a.join("assets/a.txt"), b"project a, old\n").unwrap();
    fs::write(project_b.join("assets/b.txt"), b"project b\n").unwrap();
    assert!(succeeds(&arbora_with_cache(
        &project_a,
        &shared_cache,
        &["push"]
    )));
    assert!(succeeds(&arbora_with_cache(
        &project_b,
        &shared_cache,
        &["push"]
    )));

    // Make A's old root unreachable, then GC from A. B's remote is removed so
    // its subsequent pull can succeed only if the shared cache retained B.
    fs::write(project_a.join("assets/a.txt"), b"project a, new\n").unwrap();
    assert!(succeeds(&arbora_with_cache(
        &project_a,
        &shared_cache,
        &["push"]
    )));
    let gc = arbora_with_cache(&project_a, &shared_cache, &["gc"]);
    assert!(succeeds(&gc));
    assert!(
        !String::from_utf8(gc.stdout)
            .unwrap()
            .starts_with("removed 0 ")
    );

    fs::remove_dir_all(project_b.join(".arbora-remote")).unwrap();
    fs::remove_dir_all(project_b.join("assets")).unwrap();
    assert!(succeeds(&arbora_with_cache(
        &project_b,
        &shared_cache,
        &["pull"]
    )));
    assert_eq!(
        fs::read(project_b.join("assets/b.txt")).unwrap(),
        b"project b\n"
    );
}

#[test]
fn remote_gc_is_dry_run_by_default_and_requires_confirmation_to_delete() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    assert!(succeeds(&arbora(project, &["init"])));
    fs::write(project.join("assets/data.bin"), b"old data").unwrap();
    assert!(succeeds(&arbora(project, &["push"])));
    let old_root = locked_root(project);

    fs::write(project.join("assets/data.bin"), b"new data").unwrap();
    assert!(succeeds(&arbora(project, &["push"])));

    let protected = arbora(project, &["gc", "--remote", "--keep-root", &old_root]);
    assert!(succeeds(&protected));
    assert!(
        String::from_utf8(protected.stdout)
            .unwrap()
            .contains("would delete 0 objects")
    );

    let dry_run = arbora(project, &["gc", "--remote"]);
    assert!(succeeds(&dry_run));
    assert!(
        String::from_utf8(dry_run.stdout)
            .unwrap()
            .contains("would delete 2 objects")
    );

    // A second analysis sees the same objects, proving the first invocation
    // did not mutate the remote.
    let second_dry_run = arbora(project, &["gc", "--remote", "--dry-run"]);
    assert!(succeeds(&second_dry_run));
    assert!(
        String::from_utf8(second_dry_run.stdout)
            .unwrap()
            .contains("would delete 2 objects")
    );

    let confirmed = arbora(project, &["gc", "--remote", "--confirm"]);
    assert!(succeeds(&confirmed));
    assert!(
        String::from_utf8(confirmed.stdout)
            .unwrap()
            .contains("deleted 2 objects")
    );
    let reports = fs::read_dir(project.join(".test-cache/gc-reports"))
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert!(
        reports
            .iter()
            .any(|report| report.contains("action = deleted")
                && report.contains("candidate_objects = 2")
                && report.contains("blake3:"))
    );
    let after = arbora(project, &["gc", "--remote"]);
    assert!(succeeds(&after));
    assert!(
        String::from_utf8(after.stdout)
            .unwrap()
            .contains("would delete 0 objects")
    );
}

#[test]
fn remote_gc_can_retain_every_lock_root_in_git_history() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    assert!(succeeds(&arbora(project, &["init"])));
    fs::write(project.join("assets/data.bin"), b"historical").unwrap();
    assert!(succeeds(&arbora(project, &["push"])));

    for args in [
        &["init"][..],
        &["config", "user.email", "arbora@example.invalid"],
        &["config", "user.name", "Arbora Test"],
        &["add", ".arbora.toml", ".aboraignore", "assets.lock"],
        &["commit", "-m", "retain historical root"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::write(project.join("assets/data.bin"), b"intermediate").unwrap();
    assert!(succeeds(&arbora(project, &["push"])));
    for args in [
        &["add", "assets.lock"][..],
        &["commit", "-m", "retain intermediate root"],
        &["commit", "--allow-empty", "-m", "same root, newer commit"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::write(project.join("assets/data.bin"), b"current").unwrap();
    assert!(succeeds(&arbora(project, &["push"])));
    let recent = arbora(project, &["gc", "--remote", "--keep-last", "2"]);
    assert!(succeeds(&recent));
    assert!(
        String::from_utf8(recent.stdout)
            .unwrap()
            .contains("would delete 0 objects")
    );
    let gc = arbora(project, &["gc", "--remote", "--roots-from-git"]);
    assert!(succeeds(&gc));
    assert!(
        String::from_utf8(gc.stdout)
            .unwrap()
            .contains("would delete 0 objects")
    );
}
