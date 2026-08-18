//! Worksmith CLI: `--print` one-shot, `--mode json` event stream, and a minimal
//! REPL. Full TUI is M2.

use std::io::{IsTerminal, Write, stderr, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use worksmith::agent::Agent;
use worksmith::config::Config;
use worksmith::event::{Event, EventBus};
use worksmith::llm::LlmClient;
use worksmith::llm::openai::OpenAiCompatClient;
use worksmith::memory::{MemoryStore, Scope};
use worksmith::prompt::build_system_prompt;
use worksmith::session::Session;
use worksmith::tools::{ToolContext, ToolRegistry};
use worksmith::tui::run_tui;
use worksmith::validation::CommandValidator;

#[derive(Parser, Debug)]
#[command(name = "worksmith", version, about = "A minimal terminal coding-agent harness")]
struct Args {
    /// Prompt to run. With --print/--mode json, runs one-shot; otherwise it's
    /// the first REPL turn.
    prompt: Option<String>,

    /// One-shot: run the prompt and print the final answer, then exit.
    #[arg(long)]
    print: bool,

    /// Output mode. `json` emits the typed event stream on stdout.
    #[arg(long = "mode")]
    mode: Option<String>,

    /// Override the model as `provider/model`.
    #[arg(long)]
    model: Option<String>,

    /// Resume a session by id.
    #[arg(long)]
    resume: Option<String>,

    /// Continue the most recent session for this directory.
    #[arg(long = "continue")]
    continue_: bool,

    /// Validation check: a shell command that must exit 0 for the task to be
    /// considered done. On failure its output drives a re-plan. E.g.
    /// --until "cargo test".
    #[arg(long)]
    until: Option<String>,

    /// Use the plain line-based REPL instead of the full-screen TUI.
    #[arg(long)]
    plain: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Tui,
    Repl,
    Print,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Err(e) = run(args).await {
        eprintln!("\x1b[31merror:\x1b[0m {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let config = Config::load(&cwd)?;
    let resolved = config.resolve_model(args.model.as_deref())?;

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let client: Arc<dyn LlmClient> = match resolved.provider.kind.as_str() {
        "openai-compat" => Arc::new(OpenAiCompatClient::new(
            http,
            resolved.provider.base_url.clone(),
            resolved.api_key.clone(),
        )),
        other => bail!("provider type `{other}` is not supported in M1 (use `openai-compat`)"),
    };

    let registry = Arc::new(ToolRegistry::with_builtins());
    let bus = EventBus::new();

    let mut session = open_session(&args, &cwd)?;

    let mode = if args.mode.as_deref() == Some("json") {
        OutputMode::Json
    } else if args.print {
        OutputMode::Print
    } else if args.plain || !stdout().is_terminal() {
        // The full-screen TUI needs a real terminal; fall back to the line REPL.
        OutputMode::Repl
    } else {
        OutputMode::Tui
    };

    let tool_ctx = ToolContext {
        cwd: cwd.clone(),
        session_id: session.id.clone(),
        bash_timeout: Duration::from_secs(config.bash_timeout_secs()),
    };

    let agent = Agent::new(
        client,
        registry,
        bus.clone(),
        resolved.model.clone(),
        config.temperature,
        config.max_tokens,
        config.max_steps(),
        config.max_retries(),
        config.stuck_threshold(),
        config.context_limit(),
        config.keep_recent_turns(),
        tool_ctx,
    );

    // Validation command: --until overrides the configured default.
    let validate_cmd = args.until.clone().or_else(|| config.validate_command().map(String::from));
    let bash_timeout = Duration::from_secs(config.bash_timeout_secs());

    // TUI owns its own rendering (it subscribes to the bus directly) and takes
    // ownership of the agent/session, so handle it before wiring the renderer.
    if mode == OutputMode::Tui {
        return run_tui(
            agent,
            session,
            bus,
            cwd.clone(),
            resolved.model.clone(),
            validate_cmd,
            bash_timeout,
            config.context_limit(),
            config.agents_max(),
        )
        .await;
    }

    let renderer = spawn_renderer(bus.subscribe(), mode);
    bus.emit(Event::SessionStarted { id: session.id.clone() });

    let one_shot = args.prompt.is_some() && matches!(mode, OutputMode::Print | OutputMode::Json);

    let outcome: Result<()> = if one_shot {
        let prompt = args.prompt.clone().unwrap();
        let system = build_system_prompt(&cwd, &project_store(&cwd));
        let cancel = CancellationToken::new();
        let validator = validate_cmd
            .as_ref()
            .map(|c| CommandValidator::new(c.clone(), cwd.clone(), bash_timeout));
        agent
            .run_turn(&mut session, &prompt, &system, validator.as_ref().map(|v| v as _), cancel)
            .await
            .map(|result| {
                if mode == OutputMode::Print {
                    let _ = writeln!(stdout(), "{}", result.text);
                }
            })
    } else {
        repl(&agent, &mut session, &cwd, args.prompt.clone(), &resolved.model, validate_cmd, bash_timeout)
            .await
    };

    // Drop every event-bus sender (the original *and* the agent's clone) so the
    // renderer sees the channel close, drains buffered events, and exits.
    // Without dropping `agent`, its bus clone keeps the channel open and
    // `renderer.await` hangs forever (the /quit hang).
    drop(agent);
    drop(bus);
    let _ = renderer.await;
    outcome
}

fn open_session(args: &Args, cwd: &Path) -> Result<Session> {
    if let Some(id) = &args.resume {
        let path = Session::path_for_id(id)?;
        if !path.exists() {
            bail!("no session with id {id}");
        }
        return Session::open(&path);
    }
    if args.continue_ {
        if let Some(path) = Session::most_recent_for_cwd(cwd)? {
            return Session::open(&path);
        }
        eprintln!("(no prior session for this directory; starting a new one)");
    }
    Session::create(cwd)
}

/// A memory store scoped to this project (project db in `cwd/.worksmith`).
fn project_store(cwd: &Path) -> MemoryStore {
    MemoryStore::open(Some(cwd)).unwrap_or_else(|e| {
        eprintln!("(memory unavailable: {e})");
        MemoryStore::open(None).expect("global memory")
    })
}

// ---- REPL -----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn repl(
    agent: Arc<Agent>,
    session: &mut Session,
    cwd: &Path,
    first: Option<String>,
    model: &str,
    mut validate_cmd: Option<String>,
    bash_timeout: Duration,
    config: &Config,
) -> Result<()> {
    use rustyline::DefaultEditor;
    use rustyline::error::ReadlineError;

    let mem = project_store(cwd);
    let mut workers = WorkerManager::new(agent.clone(), cwd.to_path_buf(), config.agents_max())
        .with_supervisor(config.supervisor());

    println!("worksmith — model: {model}  (/help for commands, /quit to exit)");
    if let Some(c) = &validate_cmd {
        println!("validation: `{c}` must pass before a task is considered done");
    }

    let mut editor = DefaultEditor::new().context("initializing line editor")?;
    let mut pending = first;

    loop {
        let input = match pending.take() {
            Some(p) => p,
            None => match editor.readline("worksmith\u{203a} ") {
                Ok(line) => {
                    let _ = editor.add_history_entry(line.as_str());
                    line
                }
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            },
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Bare `exit`/`quit` also leave the REPL (common muscle memory).
        if matches!(trimmed, "exit" | "quit") {
            break;
        }

        // `/compact` forces a summarization pass now.
        if trimmed == "/compact" {
            match agent.compact(session).await {
                Ok(()) => println!("(compacted)"),
                Err(e) => eprintln!("compaction error: {e:#}"),
            }
            continue;
        }

        // `/validate <cmd>` sets the success check; `/validate off` clears it.
        if let Some(rest) = trimmed.strip_prefix("/validate") {
            let rest = rest.trim();
            match rest {
                "" => match &validate_cmd {
                    Some(c) => println!("validation: `{c}`"),
                    None => println!("validation: (none) — use `/validate <command>`"),
                },
                "off" | "none" => {
                    validate_cmd = None;
                    println!("validation cleared");
                }
                cmd => {
                    validate_cmd = Some(cmd.to_string());
                    println!("validation: `{cmd}` must pass before a task is done");
                }
            }
            continue;
        }

        if let Some(cmd) = trimmed.strip_prefix('/') {
            match handle_command(cmd, &mem, session, cwd) {
                CommandResult::Quit => break,
                CommandResult::Handled => continue,
                CommandResult::NotACommand => {}
            }
        }

        let message = expand_file_mentions(trimmed, cwd);
        let system = build_system_prompt(cwd, &mem);
        let validator = validate_cmd
            .as_ref()
            .map(|c| CommandValidator::new(c.clone(), cwd.to_path_buf(), bash_timeout));

        // Ctrl+C aborts the current turn (not the program).
        let cancel = CancellationToken::new();
        let result = tokio::select! {
            r = agent.run_turn(session, &message, &system, validator.as_ref().map(|v| v as _), cancel.clone()) => r,
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                println!("\n(aborted)");
                Ok(worksmith::agent::TurnResult {
                    text: String::new(),
                    outcome: worksmith::agent::TurnOutcome::Aborted,
                })
            }
        };
        match result {
            Ok(r) if !r.outcome.is_success() => {
                println!("\x1b[2m[{}]\x1b[0m", r.outcome.label());
            }
            Ok(_) => {}
            Err(e) => eprintln!("\x1b[31mturn error:\x1b[0m {e:#}"),
        }
    }

    Ok(())
}

enum CommandResult {
    Quit,
    Handled,
    NotACommand,
}

fn handle_command(
    cmd: &str,
    mem: &MemoryStore,
    session: &mut Session,
    cwd: &Path,
) -> CommandResult {
    let mut parts = cmd.split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "quit" | "exit" | "q" => CommandResult::Quit,
        "help" | "h" => {
            println!(
                "commands:\n  \
                 /help                    this help\n  \
                 /quit                    exit\n  \
                 /new                     start a new session\n  \
                 /memory [list|global|project]  list memories\n  \
                 /memory show <id>        show a memory\n  \
                 /memory forget <id>      delete a memory\n  \
                 /memory add <scope> <kind> <subject> <content...>\n  \
                 @path                    include a file's contents in your message"
            );
            CommandResult::Handled
        }
        "new" => match Session::create(cwd) {
            Ok(s) => {
                *session = s;
                println!("(started new session {})", session.id);
                CommandResult::Handled
            }
            Err(e) => {
                eprintln!("cannot start session: {e}");
                CommandResult::Handled
            }
        },
        "memory" | "mem" => {
            handle_memory(parts, mem);
            CommandResult::Handled
        }
        _ => CommandResult::NotACommand,
    }
}

fn handle_memory<'a>(mut parts: impl Iterator<Item = &'a str>, mem: &MemoryStore) {
    let sub = parts.next().unwrap_or("list");
    match sub {
        "list" | "" => print_memories(mem, None),
        "global" => print_memories(mem, Some(Scope::Global)),
        "project" => print_memories(mem, Some(Scope::Project)),
        "show" => {
            let Some(id) = parts.next() else {
                eprintln!("usage: /memory show <id>");
                return;
            };
            match mem.get(id) {
                Ok(Some(r)) => println!(
                    "[{}] {} ({}) importance={} status={}\n{}",
                    r.scope, r.subject, r.kind, r.importance, r.status, r.content
                ),
                Ok(None) => println!("(no memory {id})"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "forget" => {
            let Some(id) = parts.next() else {
                eprintln!("usage: /memory forget <id>");
                return;
            };
            match mem.forget(id) {
                Ok(true) => println!("(forgot {id})"),
                Ok(false) => println!("(no memory {id})"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "add" => {
            let scope = parts.next().and_then(Scope::parse);
            let kind = parts.next().map(str::to_string);
            let subject = parts.next().map(str::to_string);
            let content: String = parts.collect::<Vec<_>>().join(" ");
            match (scope, kind, subject) {
                (Some(scope), Some(kind), Some(subject)) if !content.is_empty() => {
                    match mem.remember(scope, &kind, &subject, &content, 60) {
                        Ok(r) => println!("(remembered {} [{}] {})", r.id, r.kind, r.subject),
                        Err(e) => eprintln!("error: {e}"),
                    }
                }
                _ => eprintln!(
                    "usage: /memory add <global|project> <decision|constraint|preference|fact|lesson> <subject> <content...>"
                ),
            }
        }
        other => eprintln!("unknown /memory subcommand: {other}"),
    }
}

fn print_memories(mem: &MemoryStore, scope: Option<Scope>) {
    match mem.list(scope) {
        Ok(rows) if rows.is_empty() => println!("(no memories)"),
        Ok(rows) => {
            for r in rows {
                println!(
                    "{}  [{}/{}] {}  (imp {})\n    {}",
                    &r.id[..8.min(r.id.len())],
                    r.scope,
                    r.kind,
                    r.subject,
                    r.importance,
                    r.content
                );
            }
        }
        Err(e) => eprintln!("error: {e}"),
    }
}

/// Replace `@path` tokens by appending the referenced files' contents.
fn expand_file_mentions(input: &str, cwd: &Path) -> String {
    let mut appended = String::new();
    for token in input.split_whitespace() {
        if let Some(path) = token.strip_prefix('@') {
            let full = if PathBuf::from(path).is_absolute() {
                PathBuf::from(path)
            } else {
                cwd.join(path)
            };
            if let Ok(content) = std::fs::read_to_string(&full) {
                appended.push_str(&format!("\n\n<file path=\"{path}\">\n{content}\n</file>"));
            }
        }
    }
    if appended.is_empty() {
        input.to_string()
    } else {
        format!("{input}{appended}")
    }
}

// ---- rendering ------------------------------------------------------------

fn spawn_renderer(
    mut rx: tokio::sync::broadcast::Receiver<Event>,
    mode: OutputMode,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => render(&ev, mode),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn render(ev: &Event, mode: OutputMode) {
    match mode {
        OutputMode::Json => {
            if let Ok(line) = serde_json::to_string(ev) {
                let mut out = stdout();
                let _ = writeln!(out, "{line}");
            }
        }
        OutputMode::Print => render_activity(ev, true),
        OutputMode::Repl => render_activity(ev, false),
        OutputMode::Tui => {} // the TUI renders itself; this path is unused
    }
}

/// Human-facing rendering. In Print mode, tool/turn chatter goes to stderr so
/// stdout stays a clean final answer; in Repl mode everything streams to stdout.
fn render_activity(ev: &Event, print_mode: bool) {
    match ev {
        Event::Thinking { text } => {
            // Thinking is dim; shown live in the REPL, suppressed in --print.
            if !print_mode {
                let mut out = stdout();
                let _ = write!(out, "\x1b[2m{text}\x1b[0m");
                let _ = out.flush();
            }
        }
        Event::MessageDelta { text } => {
            if !print_mode {
                let mut out = stdout();
                let _ = write!(out, "{text}");
                let _ = out.flush();
            }
        }
        Event::ToolCall { name, arguments, .. } => {
            let args = truncate(arguments, 160);
            let line = format!("\n\x1b[2m⚙ {name} {args}\x1b[0m");
            emit_line(&line, print_mode);
        }
        Event::ToolResult { ok, output, .. } => {
            let status = if *ok { "ok" } else { "error" };
            let first = output.lines().next().unwrap_or("");
            let extra = output.lines().count().saturating_sub(1);
            let suffix = if extra > 0 { format!(" (+{extra} more lines)") } else { String::new() };
            let line = format!("\x1b[2m  → {status}: {}{}\x1b[0m", truncate(first, 160), suffix);
            emit_line(&line, print_mode);
        }
        Event::Nudge { reason } => {
            let line = format!("\x1b[33m↻ {reason}\x1b[0m");
            emit_line(&line, print_mode);
        }
        Event::Compaction { messages_before, messages_after } => {
            let line = format!(
                "\x1b[2m⟲ compacted context: {messages_before} → {messages_after} messages\x1b[0m"
            );
            emit_line(&line, print_mode);
        }
        Event::Validation { ok, detail } => {
            let line = if *ok {
                format!("\x1b[32m✓ validation passed: {}\x1b[0m", truncate(detail, 120))
            } else {
                format!("\x1b[31m✗ validation failed: {}\x1b[0m", truncate(detail, 200))
            };
            emit_line(&line, print_mode);
        }
        Event::Error { message } => {
            let line = format!("\x1b[31merror:\x1b[0m {message}");
            emit_line(&line, print_mode);
        }
        Event::TurnComplete { .. } if !print_mode => {
            println!();
        }
        _ => {}
    }
}

fn emit_line(line: &str, to_stderr: bool) {
    if to_stderr {
        let mut e = stderr();
        let _ = writeln!(e, "{line}");
    } else {
        let mut o = stdout();
        let _ = writeln!(o, "{line}");
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
