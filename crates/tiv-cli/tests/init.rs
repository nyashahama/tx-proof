use std::{fs, process::Command};

#[test]
fn init_binary_writes_once_and_reports_a_collision_without_overwriting() {
    let root = std::env::temp_dir().join(format!("txproof-init-cli-{}", std::process::id()));
    fs::create_dir(&root).expect("the unique test directory is created");
    fs::create_dir(root.join(".git")).expect("the Git marker is created");

    let first = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .arg("init")
        .current_dir(&root)
        .output()
        .expect("the tiv binary executes");
    assert!(
        first.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("stdout is one init report");
    assert_eq!(report["status"], "initialized");
    let original = fs::read(root.join("tiv.toml")).expect("the generated config is readable");

    let second = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .arg("init")
        .current_dir(&root)
        .output()
        .expect("the tiv binary executes again");
    assert_eq!(second.status.code(), Some(2));
    assert_eq!(
        fs::read(root.join("tiv.toml")).expect("the config remains readable"),
        original
    );

    fs::remove_dir_all(root).expect("the isolated test directory is removed");
}
