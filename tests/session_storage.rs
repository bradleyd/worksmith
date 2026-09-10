mod common;
use std::path::Path;
use worksmith::{
    llm::Message,
    session::{
        Session,
        store::{self, Date, DateRange},
    },
};

fn dated(root: &Path, date: &str, project: &Path, text: &str) -> Session {
    let dir = root
        .join(Date::parse(date).unwrap().directory())
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = Session::create_at(&dir.join("transcript.jsonl"), project).unwrap();
    session.append_message(Message::user(text)).unwrap();
    session
}
#[test]
fn calendar_handles_epoch_leap_days_and_invalid_ranges() {
    assert_eq!(Date::from_unix(0).to_string(), "1970-01-01");
    assert_eq!(Date::from_unix(951782400).to_string(), "2000-02-29");
    assert_eq!(Date::from_unix(4107542400).to_string(), "2100-03-01");
    assert_eq!(Date::from_unix(u64::MAX).to_string(), "9999-12-31");
    for date in [
        "2026-02-29",
        "2100-02-29",
        "2026-00-01",
        "2026-01-32",
        "26-1-2",
        "../../01",
    ] {
        assert!(Date::parse(date).is_err());
    }
    assert!(DateRange::new(Some("2026-09-10"), Some("2026-09-09")).is_err());
}
#[test]
fn new_session_groups_artifacts_and_resumes_with_the_same_id() {
    common::isolate_home();
    let project = tempfile::tempdir().unwrap();
    let mut session = Session::create(project.path()).unwrap();
    session
        .append_message(Message::user("write chapter 9"))
        .unwrap();
    let path = session.path().to_path_buf();
    assert_eq!(path.file_name().unwrap(), "transcript.jsonl");
    assert_eq!(
        path.parent().unwrap().file_name().unwrap(),
        session.id.as_str()
    );
    assert_eq!(Session::path_for_id(&session.id).unwrap(), path);
    assert_eq!(Session::id_from_path(&path), Some(session.id.as_str()));
    assert_eq!(
        store::artifact_path(&path, "checks", "checks"),
        path.with_file_name("checks")
    );
    assert_eq!(store::metadata(&path).unwrap().title, "write chapter 9");
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.id, session.id);
    assert_eq!(reopened.messages().len(), 1);
    assert_eq!(
        Session::most_recent_for_cwd(project.path()).unwrap(),
        Some(path.clone())
    );
    assert!(
        worksmith::mining::sessions_for_project(project.path())
            .unwrap()
            .contains(&path)
    );
    std::fs::remove_file(path.with_file_name("meta.json")).unwrap();
    assert_eq!(store::metadata(&path).unwrap().id, session.id);
}
#[tokio::test]
async fn dates_and_ripgrep_search_include_legacy_without_artifacts() {
    common::isolate_home();
    let root = worksmith::session::sessions_dir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let first = dated(&root, "2026-08-31", project.path(), "chapter 9 copper");
    let second = dated(&root, "2026-09-01", project.path(), "chapter 10 silver");
    std::fs::write(
        second.path().with_file_name("check.log"),
        "chapter 9 copper",
    )
    .unwrap();
    let legacy_path = root.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
    let mut legacy = Session::create_at(&legacy_path, project.path()).unwrap();
    legacy
        .append_message(Message::user("chapter 9 bronze"))
        .unwrap();
    let all = store::search("chapter 9", DateRange::default(), Some(project.path()))
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|item| item.path == legacy_path));
    assert_eq!(Session::path_for_id(&legacy.id).unwrap(), legacy_path);
    let range = DateRange::new(Some("2026-08-31"), Some("2026-09-01")).unwrap();
    let found = store::search("chapter 9", range, Some(project.path()))
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, first.path());
    assert!(
        store::search("[", range, Some(project.path()))
            .await
            .is_err()
    );
    assert!(
        store::search("no such chapter", range, Some(project.path()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(Session::path_for_id("../outside").is_err());
}
#[cfg(unix)]
#[test]
fn discovery_does_not_follow_symlinks() {
    common::isolate_home();
    let root = worksmith::session::sessions_dir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("hidden.jsonl"), "secret").unwrap();
    let link = root.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(outside.path().join("hidden.jsonl"), &link).unwrap();
    assert!(
        !store::transcripts(DateRange::default())
            .unwrap()
            .contains(&link)
    );
    assert!(
        store::list(DateRange::default(), Some(project.path()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn listing_and_search_cli_need_no_model_configuration() {
    common::isolate_home();
    let project = tempfile::tempdir().unwrap();
    let mut session = Session::create(project.path()).unwrap();
    session
        .append_message(Message::user("chapter 9 CLI needle"))
        .unwrap();
    for args in [
        vec!["sessions", "list"],
        vec!["sessions", "search", "chapter 9"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_worksmith"))
            .args(args)
            .arg("--project")
            .arg(project.path())
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(&session.id));
        assert!(text.contains("chapter 9 CLI needle"));
    }
}

#[cfg(unix)]
#[test]
fn project_filters_resolve_aliases_and_keep_deleted_projects_searchable() {
    common::isolate_home();
    let project = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("alias");
    std::os::unix::fs::symlink(project.path(), &alias).unwrap();
    let session = Session::create(&alias).unwrap();
    let entries = store::list(DateRange::default(), Some(project.path())).unwrap();
    assert!(entries.iter().any(|entry| entry.metadata.id == session.id));
    let gone = aliases.path().join("gone");
    std::fs::create_dir(&gone).unwrap();
    let deleted = Session::create(&gone).unwrap();
    std::fs::remove_dir(&gone).unwrap();
    let entries = store::list(DateRange::default(), Some(&gone)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].metadata.id, deleted.id);
}
