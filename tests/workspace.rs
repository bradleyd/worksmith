mod common;
use std::path::Path;
use tokio_util::sync::CancellationToken;
use worksmith::workspace::{Base, Workspace, review};

async fn git(root: &Path, args: &[&str]) -> String {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
async fn repo() -> tempfile::TempDir {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]).await;
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    )
    .await;
    git(dir.path(), &["config", "user.name", "Test"]).await;
    git(dir.path(), &["config", "commit.gpgsign", "false"]).await;
    std::fs::write(dir.path().join("file"), "base\n").unwrap();
    std::fs::write(dir.path().join("other"), "other\n").unwrap();
    git(dir.path(), &["add", "."]).await;
    git(dir.path(), &["commit", "-qm", "base"]).await;
    dir
}
async fn workspace(root: &Path) -> Workspace {
    let base = Base::capture(root).await.unwrap();
    let mut w = Workspace::new(
        base,
        &uuid::Uuid::new_v4().to_string(),
        &uuid::Uuid::new_v4().to_string(),
        Some("check".into()),
    )
    .unwrap();
    w.create(&CancellationToken::new()).await.unwrap();
    w
}
#[tokio::test]
async fn complete_results_preserve_parent_index_and_unrelated_edits() {
    let dir = repo().await;
    let root = dir.path();
    let mut w = workspace(root).await;
    let tree = w.root().unwrap();
    std::fs::write(tree.join("file"), "worker\n").unwrap();
    std::fs::write(tree.join("new binary"), [0, 255, 1, 2]).unwrap();
    git(&tree, &["add", "file"]).await;
    let paths = w.finish(Some(true), "done".into()).await.unwrap();
    assert_eq!(paths, ["file", "new binary"]);
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "base\n"
    );
    std::fs::write(root.join("other"), "parent\n").unwrap();
    git(root, &["add", "other"]).await;
    let index = git(root, &["diff", "--cached"]).await;
    let diff = review(root, "diff", Some(&w.id), false).await.unwrap();
    assert!(diff.contains("GIT binary patch") && diff.contains("+worker"));
    review(root, "apply", Some(&w.id), false).await.unwrap();
    assert_eq!(git(root, &["diff", "--cached"]).await, index);
    assert_eq!(
        std::fs::read(root.join("new binary")).unwrap(),
        [0, 255, 1, 2]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "worker\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("other")).unwrap(),
        "parent\n"
    );
    assert!(review(root, "apply", Some(&w.id), false).await.is_err());
    review(root, "discard", Some(&w.id), false).await.unwrap();
    assert!(!tree.exists());
}
#[tokio::test]
async fn overlapping_results_and_parent_changes_refuse_entire_apply() {
    let dir = repo().await;
    let root = dir.path();
    let mut a = workspace(root).await;
    let mut b = workspace(root).await;
    for (w, text) in [(&mut a, "one"), (&mut b, "two")] {
        std::fs::write(w.root().unwrap().join("file"), text).unwrap();
        std::fs::write(w.root().unwrap().join("new"), text).unwrap();
        w.finish(None, "done".into()).await.unwrap();
    }
    review(root, "apply", Some(&a.id), false).await.unwrap();
    assert!(review(root, "apply", Some(&b.id), false).await.is_err());
    assert_eq!(std::fs::read_to_string(root.join("new")).unwrap(), "one");
    assert_eq!(
        std::fs::read_to_string(b.root().unwrap().join("new")).unwrap(),
        "two"
    );
    assert!(review(root, "discard", Some(&b.id), false).await.is_err());
    review(root, "discard", Some(&b.id), true).await.unwrap();
    review(root, "discard", Some(&a.id), true).await.unwrap();
}
#[tokio::test]
async fn dirty_start_stale_result_new_file_collision_and_moved_head_are_rejected() {
    let dir = repo().await;
    let root = dir.path();
    let mut w = workspace(root).await;
    std::fs::write(w.root().unwrap().join("new"), "worker").unwrap();
    w.finish(None, "done".into()).await.unwrap();
    std::fs::write(root.join("new"), "parent").unwrap();
    assert!(Base::capture(root).await.is_err());
    assert!(review(root, "apply", Some(&w.id), false).await.is_err());
    std::fs::remove_file(root.join("new")).unwrap();
    std::fs::write(w.root().unwrap().join("new"), "later edit").unwrap();
    assert!(
        review(root, "apply", Some(&w.id), false)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since")
    );
    std::fs::write(w.root().unwrap().join("new"), "worker").unwrap();
    git(root, &["commit", "--allow-empty", "-qm", "moved"]).await;
    assert!(
        review(root, "apply", Some(&w.id), false)
            .await
            .unwrap_err()
            .to_string()
            .contains("HEAD changed")
    );
    review(root, "discard", Some(&w.id), true).await.unwrap();
}
#[tokio::test]
async fn interrupted_work_is_retained_and_running_workspace_is_locked() {
    let dir = repo().await;
    let w = workspace(dir.path()).await;
    let lock = w.lock().unwrap();
    std::fs::write(w.root().unwrap().join("file"), "interrupted").unwrap();
    assert!(
        review(dir.path(), "diff", Some(&w.id), false)
            .await
            .is_err()
    );
    drop(lock);
    let text = review(dir.path(), "diff", Some(&w.id), false)
        .await
        .unwrap();
    assert!(text.contains("interrupted worker") && text.contains("+interrupted"));
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn symlinks_cannot_be_applied() {
    let dir = repo().await;
    let mut w = workspace(dir.path()).await;
    std::os::unix::fs::symlink(dir.path().join("other"), w.root().unwrap().join("link")).unwrap();
    w.finish(None, "done".into()).await.unwrap();
    assert!(
        review(dir.path(), "apply", Some(&w.id), false)
            .await
            .is_err()
    );
    assert!(!dir.path().join("link").exists());
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn executable_modes_deletions_and_later_unapplied_edits_are_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repo().await;
    let mut w = workspace(dir.path()).await;
    std::fs::set_permissions(
        w.root().unwrap().join("file"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::remove_file(w.root().unwrap().join("other")).unwrap();
    w.finish(Some(true), "done".into()).await.unwrap();
    review(dir.path(), "apply", Some(&w.id), false)
        .await
        .unwrap();
    assert_ne!(
        std::fs::metadata(dir.path().join("file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(!dir.path().join("other").exists());
    std::fs::write(w.root().unwrap().join("later"), "keep").unwrap();
    assert!(
        review(dir.path(), "discard", Some(&w.id), false)
            .await
            .is_err()
    );
    assert!(w.root().unwrap().join("later").exists());
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn staged_parent_edits_block_apply_even_when_working_file_matches_base() {
    let dir = repo().await;
    let mut w = workspace(dir.path()).await;
    std::fs::write(w.root().unwrap().join("file"), "worker").unwrap();
    w.finish(None, "done".into()).await.unwrap();
    std::fs::write(dir.path().join("file"), "staged parent").unwrap();
    git(dir.path(), &["add", "file"]).await;
    std::fs::write(dir.path().join("file"), "base\n").unwrap();
    let index = git(dir.path(), &["diff", "--cached"]).await;
    assert!(
        review(dir.path(), "apply", Some(&w.id), false)
            .await
            .is_err()
    );
    assert_eq!(index, git(dir.path(), &["diff", "--cached"]).await);
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn interrupted_apply_is_never_retried_automatically() {
    let dir = repo().await;
    let mut w = workspace(dir.path()).await;
    std::fs::write(w.root().unwrap().join("file"), "worker").unwrap();
    w.finish(None, "done".into()).await.unwrap();
    w.state = worksmith::workspace::State::Applying;
    std::fs::write(
        w.root().unwrap().parent().unwrap().join("record.json"),
        serde_json::to_vec(&w).unwrap(),
    )
    .unwrap();
    assert!(
        review(dir.path(), "apply", Some(&w.id), false)
            .await
            .unwrap_err()
            .to_string()
            .contains("interrupted applies")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "base\n"
    );
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_setup_can_be_inspected_and_explicitly_discarded() {
    let dir = repo().await;
    let mut w = Workspace::new(
        Base::capture(dir.path()).await.unwrap(),
        &uuid::Uuid::new_v4().to_string(),
        &uuid::Uuid::new_v4().to_string(),
        None,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(w.create(&cancel).await.is_err());
    assert!(
        review(dir.path(), "retained", None, false)
            .await
            .unwrap()
            .contains(&w.id)
    );
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn assume_unchanged_cannot_hide_parent_edits_from_apply() {
    let dir = repo().await;
    let mut w = workspace(dir.path()).await;
    std::fs::write(w.root().unwrap().join("file"), "worker").unwrap();
    w.finish(None, "done".into()).await.unwrap();
    git(dir.path(), &["update-index", "--assume-unchanged", "file"]).await;
    std::fs::write(dir.path().join("file"), "hidden parent edit").unwrap();
    assert!(
        review(dir.path(), "apply", Some(&w.id), false)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "hidden parent edit"
    );
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn nonoverlapping_hunks_in_a_parent_modified_file_are_not_applied() {
    let dir = repo().await;
    let base = format!("first\n{}last\n", "unchanged\n".repeat(20));
    std::fs::write(dir.path().join("file"), &base).unwrap();
    git(dir.path(), &["add", "file"]).await;
    git(dir.path(), &["commit", "-qm", "long file"]).await;
    let mut w = workspace(dir.path()).await;
    std::fs::write(
        w.root().unwrap().join("file"),
        base.replacen("first", "worker", 1),
    )
    .unwrap();
    w.finish(None, "done".into()).await.unwrap();
    let parent = base.replace("last", "parent");
    std::fs::write(dir.path().join("file"), &parent).unwrap();
    assert!(
        review(dir.path(), "apply", Some(&w.id), false)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        parent
    );
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn discard_refuses_a_replaced_workspace_root_and_preserves_other_files() {
    let dir = repo().await;
    let other = repo().await;
    let mut w = workspace(dir.path()).await;
    w.finish(None, "done".into()).await.unwrap();
    let root = w.root().unwrap();
    let saved = root.with_file_name("saved-tree");
    std::fs::rename(&root, &saved).unwrap();
    std::os::unix::fs::symlink(other.path(), &root).unwrap();
    assert!(
        review(dir.path(), "discard", Some(&w.id), true)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(other.path().join("file")).unwrap(),
        "base\n"
    );
    std::fs::remove_file(&root).unwrap();
    std::fs::rename(saved, &root).unwrap();
    review(dir.path(), "discard", Some(&w.id), true)
        .await
        .unwrap();
}
