mod common;
// Tests for the CommandValidator (the success-predicate behind --until).

use std::time::Duration;

use worksmith::validation::{CommandValidator, Validator};

#[tokio::test]
async fn command_validator_passes_on_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new("true", dir.path().to_path_buf(), Duration::from_secs(10));
    assert!(v.validate().await.unwrap().passed);
}

#[tokio::test]
async fn command_validator_fails_with_output() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
        "echo boom >&2; exit 1",
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );
    let result = v.validate().await.unwrap();
    assert!(!result.passed);
    assert_eq!(result.exit_code, Some(1));
    assert!(result.stderr.contains("boom"));
}

#[tokio::test]
async fn command_validator_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
        "sleep 5",
        dir.path().to_path_buf(),
        Duration::from_millis(200),
    );
    let err = v.validate().await.unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_stops_validation_descendants_before_they_write() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let v = CommandValidator::new(
        "(sleep 0.5; echo survived > late) & echo ready > ready; wait",
        dir.path().to_path_buf(),
        Duration::from_secs(5),
    )
    .with_cancel(cancel.clone());
    let root = dir.path().to_path_buf();
    let trigger = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !root.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
    };
    let (result, _) = tokio::join!(v.validate(), trigger);
    assert!(result.unwrap_err().contains("cancelled"));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!root.join("late").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn successful_validation_does_not_leave_background_writers() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
        "(sleep 0.5; echo survived > late) >/dev/null 2>&1 & true",
        dir.path().to_path_buf(),
        Duration::from_secs(5),
    );
    assert!(v.validate().await.unwrap().passed);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!dir.path().join("late").exists());
}

#[tokio::test]
async fn successful_output_is_saved_with_a_bounded_utf8_excerpt() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    let original = format!("BEGIN\n{}\nRan 133 tests\nOK", "é".repeat(5000));
    let check = worksmith::validation::CheckReport::record(
        "test command".into(),
        Ok(worksmith::validation::CheckOutput {
            passed: true,
            exit_code: Some(0),
            stdout: original.clone(),
            stderr: "stderr marker".into(),
        }),
        &session,
    )
    .await;
    assert!(check.output.starts_with("…(earlier output truncated)…"));
    assert!(check.output.contains("Ran 133 tests\nOK"));
    let log = std::fs::read_to_string(check.log_path.as_ref().unwrap()).unwrap();
    assert!(log.contains(&original));
    assert!(log.contains("stderr marker"));
    let restored: worksmith::validation::CheckReport =
        serde_json::from_str(&serde_json::to_string(&check).unwrap()).unwrap();
    assert_eq!(restored.display(), check.display());
    assert!(!check.display().contains("Some("));
    let next = worksmith::validation::CheckReport::record(
        "test command".into(),
        Err("failed again".into()),
        &session,
    )
    .await;
    assert_ne!(next.log_path, check.log_path);
    assert!(check.log_path.unwrap().exists());
}

#[tokio::test]
async fn passing_command_keeps_both_output_streams() {
    let dir = tempfile::tempdir().unwrap();
    let check = CommandValidator::new(
        "echo out; echo err >&2",
        dir.path().into(),
        Duration::from_secs(5),
    )
    .validate()
    .await
    .unwrap();
    assert!(check.passed);
    assert_eq!(check.stdout, "out\n");
    assert_eq!(check.stderr, "err\n");
}

#[test]
fn old_validation_events_still_deserialize() {
    let event: worksmith::event::Event =
        serde_json::from_str(r#"{"type":"validation","ok":true,"detail":"old check"}"#).unwrap();
    assert!(matches!(
        event,
        worksmith::event::Event::Validation { report: None, .. }
    ));
}
