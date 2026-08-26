//! First-run behaviour, in its own test binary on purpose.
//!
//! This is the one test that must NOT share a `WORKSMITH_HOME` with anything
//! else: it points the variable at a directory that does not exist yet, and the
//! rest of the suite pins one shared scratch home per process via `OnceLock`.
//! Cargo gives each integration test file its own process, which is the
//! isolation this needs.

/// First run: `~/.worksmith` does not exist yet. Every downstream error names a
/// `config.toml` in that directory, so the directory has to exist and the
/// annotated reference has to be sitting in it.
#[test]
fn a_fresh_global_home_is_created_with_a_reference_config() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("never-created");
    assert!(!home.exists(), "precondition: the global dir is missing");
    unsafe { std::env::set_var(worksmith::config::GLOBAL_DIR_ENV, &home) };

    let project = tempfile::tempdir().unwrap();
    let cfg = worksmith::config::Config::load(project.path()).unwrap();

    assert!(home.is_dir(), "the global dir must be created on first load");
    let example = home.join(worksmith::config::EXAMPLE_CONFIG);
    assert!(example.is_file(), "an annotated example must be seeded beside it");
    let body = std::fs::read_to_string(&example).unwrap();
    assert!(body.contains("[providers."), "the example must show a provider section");

    // Seeding a reference is not the same as choosing a model: writing
    // config.toml would pick a provider on the user's behalf.
    assert!(!home.join("config.toml").exists(), "no config.toml is invented");

    // And the error a fresh user hits must name both paths, not just "config.toml".
    let err = format!("{:#}", cfg.resolve_model(None).unwrap_err());
    assert!(err.contains(&home.display().to_string()), "error names the real path: {err}");
    assert!(err.contains("config.example.toml"), "error points at the example: {err}");
}

/// The stall guard is per-provider, because the right number is the endpoint's:
/// a loaded local server takes minutes to produce a first token, and OpenRouter
/// does not.
#[test]
fn a_stream_idle_timeout_is_per_provider_with_a_generous_default() {
    let c: worksmith::config::Config = toml::from_str(
        r#"
        model = "local/m"
        [providers.local]
        base-url = "http://127.0.0.1:8000/v1"
        [providers.remote]
        base-url = "https://openrouter.ai/api/v1"
        stream-idle-timeout = 90
        "#,
    )
    .unwrap();

    assert_eq!(c.providers["local"].stream_idle_timeout, None, "falls back to the default");
    assert_eq!(c.providers["remote"].stream_idle_timeout, Some(90));
}

/// The window the server serves and the window the config claims are two
/// different numbers, and a mismatch is invisible until a turn has already gone
/// badly — silent when it is too low, late when it is too high.
#[tokio::test]
async fn a_context_mismatch_is_reported_in_both_directions() {
    use worksmith::llm::warn_on_context_mismatch;

    // No server: best-effort, never an error, never a false alarm.
    let http = reqwest::Client::new();
    let none =
        warn_on_context_mismatch(&http, "http://127.0.0.1:1/v1", "some/model", 65_536).await;
    assert!(none.is_none(), "an unreachable server is not a misconfiguration");
}

/// Building a `reqwest::Client` is synchronous and, with rustls-native-certs on
/// macOS, reads the system keychain — 8 seconds cold, with the runtime blocked
/// throughout, so no timer can interrupt it. The probe therefore takes a client
/// instead of building one, and startup spawns it rather than awaiting it.
#[tokio::test]
async fn the_probe_reuses_a_client_and_never_builds_its_own() {
    use tokio::net::TcpListener;
    use worksmith::llm::warn_on_context_mismatch;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });

    // A caller-supplied client carries the caller's timeouts; a silent server
    // is answered with None, not a hang and not a false alarm.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(300))
        .build()
        .unwrap();
    let out =
        warn_on_context_mismatch(&http, &format!("http://127.0.0.1:{port}/v1"), "m", 65_536).await;
    assert!(out.is_none(), "a silent server is not a misconfiguration");
}

#[test]
fn a_context_mismatch_is_caught_in_both_directions() {
    use worksmith::llm::context_mismatch;

    // The real case: vLLM serving 65536 while the config claimed 128000, which
    // halved the footer's gauge and made compaction look early.
    let over = context_mismatch("m", 65_536, 128_000).expect("over-declared must warn");
    assert!(over.contains("65536"), "names the number to use: {over}");
    assert!(over.contains("rejected"), "says what goes wrong: {over}");

    let under = context_mismatch("m", 65_536, 32_768).expect("under-declared must warn");
    assert!(under.contains("65536"), "names the number to use: {under}");
    assert!(under.contains("compaction fires early"), "says what goes wrong: {under}");

    // Rounding is not a mismatch: 128000 against a served 131072 is someone
    // being approximate, not someone being wrong.
    assert!(context_mismatch("m", 131_072, 128_000).is_none(), "128000 vs 131072 is fine");
    assert!(context_mismatch("m", 65_536, 65_536).is_none(), "exact agreement is fine");
}
