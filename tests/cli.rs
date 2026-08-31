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
