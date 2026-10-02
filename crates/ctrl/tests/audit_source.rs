#![allow(clippy::unwrap_used)]

use std::{fs, path::Path, process::Command};

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn untracked_build_input_must_make_source_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    git(
        tmp.path(),
        &["config", "user.email", "audit@example.invalid"],
    );
    git(tmp.path(), &["config", "user.name", "Audit"]);
    fs::write(
        tmp.path().join("Russelfile.toml"),
        "[service]\nname='audit'\n",
    )
    .unwrap();
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-qm", "fixture"]);

    assert!(
        !russel_ctrl::git::source_rev(tmp.path(), "Russelfile.toml")
            .await
            .unwrap()
            .dirty
    );
    fs::write(tmp.path().join("index.html"), "an untracked build input").unwrap();
    assert!(
        russel_ctrl::git::source_rev(tmp.path(), "Russelfile.toml")
            .await
            .unwrap()
            .dirty,
        "untracked build input recorded as clean"
    );
}
