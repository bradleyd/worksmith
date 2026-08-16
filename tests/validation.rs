//! Tests for the CommandValidator (the success-predicate behind --until).

use std::time::Duration;

use worksmith::validation::{CommandValidator, Validator};

#[tokio::test]
async fn command_validator_passes_on_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new("true", dir.path().to_path_buf(), Duration::from_secs(10));
    assert!(v.validate().await.is_ok());
}

#[tokio::test]
async fn command_validator_fails_with_output() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new(
        "echo boom >&2; exit 1",
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );
    let err = v.validate().await.unwrap_err();
    assert!(err.contains("exit code 1"), "got: {err}");
    assert!(err.contains("boom"), "should include command output: {err}");
}

#[tokio::test]
async fn command_validator_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let v = CommandValidator::new("sleep 5", dir.path().to_path_buf(), Duration::from_millis(200));
    let err = v.validate().await.unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
}
