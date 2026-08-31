use std::{fs, process::Command};

fn arbora(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_arbora"))
        .env("XDG_CACHE_HOME", project.join(".test-cache"))
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
