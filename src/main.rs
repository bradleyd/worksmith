//! Worksmith CLI: `--print` one-shot, `--mode json` event stream, and a minimal
//! REPL. Full TUI is M2.

use std::io::{IsTerminal, Write, stderr, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use worksmith::agent::Agent;
use worksmith::config::Config;
use worksmith::event::{Event, EventBus};
use worksmith::memory::{MemoryStore, Scope};
use worksmith::prompt::{build_system_prompt, build_worker_prompt};
use worksmith::session::Session;
use worksmith::tools::{ToolContext, ToolRegistry};
use worksmith::tui::run_tui;
use worksmith::validation::CommandValidator;
use worksmith::worker::{WorkerManager, WorkerStatus};

#[derive(Parser, Debug)]
#[command(name = "worksmith", version, about = "A minimal terminal coding-agent harness")]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Prompt to run. With --print/--mode json, runs one-shot; otherwise it's
    /// the first REPL turn.
    prompt: Option<String>,

    /// One-shot: run the prompt and print the final answer, then exit.
    #[arg(long)]
    print: bool,

    /// Output mode. `json` emits the typed event stream on stdout.
    #[arg(long = "mode", global = true)]
    mode: Option<String>,

    /// Override the session's model as `provider/model`.
    #[arg(long, global = true)]
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

    /// Feeling lucky: answer without thinking first. Much cheaper and quicker
    /// (500 completion tokens vs 31 on a small Qwen for the same question);
    /// the validation loop is what catches the difference.
    #[arg(long, global = true)]
    fast: bool,

    /// Trust this project's `.worksmith/config.toml` without asking. A project
    /// config can run shell commands and redirect model traffic, so it is
    /// otherwise ignored until you say yes once.
    #[arg(long, global = true)]
    trust_project: bool,

    /// Approve every command without asking. Outward-facing actions (git push,
    /// sudo, publishing, sending data) otherwise prompt in the TUI and are
    /// refused when nothing can prompt. Use for unattended runs you trust.
    #[arg(long, global = true)]
    approve_all: bool,

    /// Force thinking on, overriding `agent.thinking` in config. Takes an
    /// optional token budget (`--think 2000`), which caps the reasoning alone so
    /// it cannot consume all of `max-tokens` and leave nothing for the answer.
    #[arg(
        long,
        global = true,
        conflicts_with = "fast",
        num_args = 0..=1,
        default_missing_value = "on",
        value_name = "on|off|TOKENS"
    )]
    think: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a task in background workers and print what they produced.
    ///
    /// The non-interactive form of `/spawn`: fans out, waits, reports each
    /// worker, then (unless --no-synthesis) has the session's model combine
    /// the results into one answer.
    Spawn {
        /// Exactly N workers. Omit to let a planner decide how many.
        #[arg(short = 'n', long = "count")]
        count: Option<usize>,

        /// One worker per file whose *name* matches this regex.
        #[arg(long)]
        each_files: Option<String>,

        /// Model the workers run on, as `provider/model`. Overrides
        /// `agents.model`. The synthesis still runs on the session's model,
        /// so `--model` and `--worker-model` are deliberately separate.
        #[arg(long = "worker-model")]
        worker_model: Option<String>,

        /// Print each worker's result and stop; don't run the combining turn.
        /// Use this when the judge needs a model that can't be resident at the
        /// same time as the workers' (swap models between the two commands).
        #[arg(long)]
        no_synthesis: bool,

        /// The task to delegate.
        task: String,
    },
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
    let config = resolve_project_trust(Config::load(&cwd)?, &cwd, &args)?;
    let resolved = config.resolve_model(args.model.as_deref())?;

    let client = worksmith::llm::client_for(&resolved)?;

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

    if let Some(Cmd::Spawn { .. }) = &args.cmd {
        // The renderer has to exist before the work starts, or `--mode json`
        // reports a silent, tokenless run — which is exactly what it did.
        let renderer = spawn_renderer(bus.subscribe(), mode);
        let outcome =
            run_spawn(&args, &config, &resolved, client, registry, bus.clone(), session, &cwd)
                .await;
        drop(bus);
        let _ = renderer.await;
        return outcome;
    }

    // Who answers "may I push?" depends on whether anyone is watching. The TUI
    // can put the question on screen; a --print run cannot, and defaults to no.
    let (approver, approvals): (Arc<dyn worksmith::tools::approval::Approver>, _) =
        if args.approve_all {
            (Arc::new(worksmith::tools::approval::AutoApprove), None)
        } else if mode == OutputMode::Tui {
            let (a, rx) = worksmith::tools::approval::ChannelApprover::new();
            (Arc::new(worksmith::tools::approval::RememberingApprover::new(Arc::new(a))), Some(rx))
        } else {
            (Arc::new(worksmith::tools::approval::RefuseWhenUnattended), None)
        };

    let tool_ctx = ToolContext {
        cwd: cwd.clone(),
        session_id: session.id.clone(),
        bash_timeout: Duration::from_secs(config.bash_timeout_secs()),
        is_worker: false,
        approver,
    };

    // --fast / --think beat the configured default.
    let thinking = resolve_thinking(&args, &config)?;
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
        resolved.settings.context.unwrap_or_else(|| config.context_limit()),
        config.keep_recent_turns(),
        tool_ctx,
    )
    .with_sampling(
        resolved.settings.temperature,
        resolved.settings.top_p,
        resolved.settings.top_k,
    )
    .with_thinking(thinking);

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
            config.supervisor(),
            config.fanout_auto(),
            config.synthesize(),
            config.clone(),
            resolved.settings.clone(),
            approvals.expect("the TUI branch always builds an approval channel"),
        )
        .await;
    }

    // Workers need a shared handle to the agent; the TUI path already owns it.
    let agent = Arc::new(agent);
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
        repl(
            agent.clone(),
            &mut session,
            &cwd,
            args.prompt.clone(),
            &resolved.model,
            validate_cmd,
            bash_timeout,
            &config,
        )
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

/// `worksmith spawn` — fan out to background workers, wait, report.
///
/// This is what makes the worker layer measurable from a script: the TUI's
/// `/spawn` needs a terminal, so nothing could exercise workers headlessly.
#[allow(clippy::too_many_arguments)]
async fn run_spawn(
    args: &Args,
    config: &Config,
    resolved: &worksmith::config::ResolvedModel,
    client: Arc<dyn worksmith::llm::LlmClient>,
    registry: Arc<ToolRegistry>,
    bus: EventBus,
    mut session: Session,
    cwd: &Path,
) -> Result<()> {
    use worksmith::fanout::{assign, matching_files, plan_fanout};
    use worksmith::llm::ModelOverride;
    use worksmith::worker::WorkerManager;

    let Some(Cmd::Spawn { count, each_files, worker_model: model, no_synthesis, task }) =
        &args.cmd
    else {
        unreachable!("run_spawn is only called for the spawn subcommand");
    };
    let json = args.mode.as_deref() == Some("json");

    let tool_ctx = ToolContext {
        cwd: cwd.to_path_buf(),
        session_id: session.id.clone(),
        bash_timeout: Duration::from_secs(config.bash_timeout_secs()),
        is_worker: false,
        ..Default::default()
    };
    let agent = Arc::new(Agent::new(
        client,
        registry,
        bus.clone(),
        resolved.model.clone(),
        config.temperature,
        config.max_tokens,
        config.max_steps(),
        config.max_retries(),
        config.stuck_threshold(),
        resolved.settings.context.unwrap_or_else(|| config.context_limit()),
        config.keep_recent_turns(),
        tool_ctx,
    )
    .with_sampling(
        resolved.settings.temperature,
        resolved.settings.top_p,
        resolved.settings.top_k,
    )
    .with_thinking(resolve_thinking(args, config)?));

    let mem = project_store(cwd);
    let system = build_worker_prompt(cwd, &mem);
    let over = match model.as_deref().or_else(|| config.agents_model()) {
        Some(spec) => Some(ModelOverride::resolve(config, spec)?),
        None => None,
    };
    let worker_model = over.as_ref().map(|o| o.model.clone());

    // Decide the work: one worker per matching file, or a planner split.
    let tasks: Vec<String> = match each_files {
        Some(pattern) => {
            let files = matching_files(cwd, pattern).map_err(anyhow::Error::msg)?;
            if files.is_empty() {
                bail!("no files match `{pattern}`");
            }
            files.iter().map(|f| assign(task, f)).collect()
        }
        None => {
            let plan = plan_fanout(agent.clone(), task.clone(), *count, config.agents_max()).await;
            // Always visible, json mode or not: this is the step that decides
            // what every worker does, and it used to be a black box.
            eprintln!("{}", plan.note);
            plan.tasks
        }
    };

    if !json {
        if let Some(m) = &worker_model {
            eprintln!("workers on {m}");
        }
        if tasks.len() > 1 {
            for (i, t) in tasks.iter().enumerate() {
                eprintln!("  {}. {}", i + 1, t.lines().next().unwrap_or(t));
            }
        }
    }

    let mut workers = WorkerManager::new(agent.clone(), cwd.to_path_buf(), config.agents_max())
        .with_default_validate(
            config.agents_validate().map(str::to_string),
            Duration::from_secs(config.bash_timeout_secs()),
        )
        .with_supervisor(config.supervisor());
    let report = workers.spawn_many_on(tasks, system, task.clone(), over);
    let expected = report.started.len() + report.queued;
    if expected == 0 {
        bail!("no workers started");
    }

    // Wait them out, reporting each as it lands.
    let mut done: Vec<worksmith::worker::WorkerSummary> = Vec::new();
    while done.len() < expected {
        for id in workers.pump() {
            if !json {
                eprintln!("(started {id} from the queue)");
            }
        }
        for w in workers.take_newly_finished() {
            if !json {
                eprintln!("{}", worksmith::report::worker_headline(&w));
            }
            done.push(w);
        }
        if done.len() < expected {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Workers run on their own buses, so their spend never reaches this
    // process's event stream. Re-emit the total so `--mode json` consumers
    // (the eval harness) can account for it.
    let worker_tokens: u32 = done.iter().map(|w| w.tokens).sum();
    bus.emit(Event::Usage {
        prompt_tokens: 0,
        completion_tokens: worker_tokens,
        total_tokens: worker_tokens,
        reasoning_tokens: 0,
        finish_reason: None,
    });

    let group = worksmith::report::GroupAcc {
        group: report.group.unwrap_or(0),
        request: task.clone(),
        total: done.len(),
        done,
    };
    let body = worksmith::report::group_report(&group);

    // Nothing to combine. Asking a model to "combine their results" when every
    // worker failed does not produce an error, it produces an agentic turn that
    // tries to do the whole job itself, for up to max-steps. That reads as a
    // hang and costs real tokens. Observed with three workers 401ing against a
    // local endpoint.
    let succeeded = group.done.iter().filter(|w| w.status == WorkerStatus::Done).count();
    if succeeded == 0 {
        let _ = writeln!(stdout(), "{body}");
        // Name the reasons here rather than pointing at output above: worker
        // headlines are only printed when *not* in --mode json, so in the mode
        // an eval harness uses there was nothing above at all. Three separate
        // investigations went to the session files for want of this line.
        let why: Vec<String> = group
            .done
            .iter()
            .map(|w| {
                let reason = w
                    .escalation
                    .clone()
                    .unwrap_or_else(|| w.last.lines().next().unwrap_or("").to_string());
                format!("{} [{}] {}", w.id, w.status.label(), reason)
            })
            .collect();
        bail!(
            "all {} workers failed; nothing to synthesize:\n  {}",
            group.done.len(),
            why.join("\n  ")
        );
    }

    if *no_synthesis || !config.synthesize() || succeeded < 2 {
        let _ = writeln!(stdout(), "{body}");
        return Ok(());
    }

    // The parent decides. This is the whole point of the split: cheap doers,
    // one smarter judgment at the end.
    session.append_message(worksmith::llm::Message::user(body))?;
    // One-shot: this command prints and exits, so a question at the end reaches
    // nobody. Observed: a synthesis turn that ended "want me to apply that
    // sourcing pass?" and then exited, with no way to say yes.
    let ask = format!(
        "Your {} background workers just reported back (above). Combine their results into \
         one answer to the original request: {}\n\n\
         This runs non-interactively and your answer is printed as the final output. \
         Do not ask follow-up questions or offer to do more work; give the finished answer.",
        succeeded,
        group.request
    );
    let sys = build_system_prompt(cwd, &mem);
    let result = agent
        .run_turn(&mut session, &ask, &sys, None, CancellationToken::new())
        .await?;
    let _ = writeln!(stdout(), "{}", result.text);
    Ok(())
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
    let worker_model = match config.agents_model() {
        Some(spec) => match worksmith::llm::ModelOverride::resolve(config, spec) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("agents.model `{spec}` is unusable: {e:#}");
                None
            }
        },
        None => None,
    };
    let mut workers = WorkerManager::new(agent.clone(), cwd.to_path_buf(), config.agents_max())
        .with_default_validate(
            config.agents_validate().map(str::to_string),
            Duration::from_secs(config.bash_timeout_secs()),
        )
        .with_supervisor(config.supervisor())
        .with_default_model(worker_model);

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
            drain_workers(&mut workers, session);
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
            match handle_command(cmd, &mem, session, cwd, &mut workers, &agent, config).await {
                CommandResult::Quit => break,
                CommandResult::Handled => {
                    drain_workers(&mut workers, session);
                    continue;
                }
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
        drain_workers(&mut workers, session);
    }

    Ok(())
}

/// Start anything queued, then print and record whatever finished. The plain
/// REPL has no event loop, so this runs at the natural pauses instead.
/// Results are appended to the session so the next turn can use them — the
/// same contract as the TUI, minus the automatic synthesis turn (there's no
/// loop here to fire one while you're at the prompt).
fn drain_workers(workers: &mut WorkerManager, session: &mut Session) {
    for id in workers.pump() {
        println!("(started {id} from the queue)");
    }
    for w in workers.take_newly_finished() {
        println!("{}", worksmith::report::worker_headline(&w));
        let report = worksmith::report::single_report(&w);
        if let Err(e) = session.append_message(worksmith::llm::Message::user(report)) {
            eprintln!("(could not record the worker's result: {e})");
        }
    }
}

/// The tail of a session as plain text for the memory classifier. Tool output
/// is dropped — it's exactly what must never become durable memory.
fn render_recent(session: &Session, max_messages: usize) -> String {
    use worksmith::llm::Role;
    let msgs = session.messages();
    let start = msgs.len().saturating_sub(max_messages);
    let mut out = String::new();
    for m in &msgs[start..] {
        let role = match m.role {
            Role::System | Role::Tool => continue,
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        if let Some(c) = &m.content
            && !c.trim().is_empty()
        {
            let body: String = c.chars().take(2_000).collect();
            out.push_str(&format!("[{role}] {body}\n"));
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("[{role} called {}]\n", tc.name));
        }
    }
    out
}

enum CommandResult {
    Quit,
    Handled,
    NotACommand,
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd: &str,
    mem: &MemoryStore,
    session: &mut Session,
    cwd: &Path,
    workers: &mut WorkerManager,
    agent: &Arc<Agent>,
    config: &Config,
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
                 /memory search <query>   search memories\n  \
                 /memory extract          distill this session into memories\n  \
                 /memory mine [n]         mine past sessions of this project\n  \
                 /memory pending | /memory approve <id>   review proposals\n  \
                 /knowledge [index|search <query>|status]  the project's own text\n  \
                 /skill [name]            list skills, or load one\n  \
                 /spawn [-n N | --each-files <regex>] <task>   background worker(s)\n  \
                 /agents [list|show <id>|kill <id>|nudge <id> <msg>|drop-queued]\n  \
                 /validate <cmd|off>      success check for a turn\n  \
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
            handle_memory(parts, mem, agent, session).await;
            CommandResult::Handled
        }
        "knowledge" | "know" => {
            handle_knowledge(parts, cwd);
            CommandResult::Handled
        }
        "skill" | "skills" => {
            handle_skill(parts, cwd);
            CommandResult::Handled
        }
        "spawn" => {
            handle_spawn(cmd, cwd, mem, workers, agent, config).await;
            CommandResult::Handled
        }
        "agents" | "workers" => {
            handle_agents(parts, workers);
            CommandResult::Handled
        }
        _ => CommandResult::NotACommand,
    }
}

/// `/spawn` in the line REPL. The planner call blocks the prompt, which is fine
/// here — unlike the TUI there's no frame to keep drawing.
async fn handle_spawn(
    cmd: &str,
    cwd: &Path,
    mem: &MemoryStore,
    workers: &mut WorkerManager,
    agent: &Arc<Agent>,
    config: &Config,
) {
    use worksmith::fanout::{
        FanOut, assign, fanout_notice, matching_files, parse_spawn, plan_fanout, spawn_notice,
    };

    let args = cmd.strip_prefix("spawn").unwrap_or("").trim();
    let req = match parse_spawn(args, config.fanout_auto()) {
        Ok(r) => r,
        Err(msg) => {
            println!("{msg}");
            return;
        }
    };
    let system = build_worker_prompt(cwd, mem);
    let over = match req.model.as_deref() {
        Some(spec) => match worksmith::llm::ModelOverride::resolve(config, spec) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("--model `{spec}`: {e:#}");
                return;
            }
        },
        None => None,
    };

    let (tasks, request) = match req.fanout {
        FanOut::Files(pattern) => match matching_files(cwd, &pattern) {
            Ok(files) if files.is_empty() => {
                println!("no files match `{pattern}`");
                return;
            }
            Ok(files) => (
                files.iter().map(|f| assign(&req.task, f)).collect::<Vec<_>>(),
                req.task.clone(),
            ),
            Err(e) => {
                eprintln!("{e}");
                return;
            }
        },
        FanOut::Count(1) => {
            match workers.spawn_checked(req.task.clone(), system, over, req.validate.clone()) {
                Ok(outcome) => println!("{}", spawn_notice(&outcome, &req.task)),
                Err(e) => eprintln!("spawn failed: {e}"),
            }
            return;
        }
        FanOut::Count(n) => {
            println!("(planning fan-out…)");
            let plan =
                plan_fanout(agent.clone(), req.task.clone(), Some(n), config.agents_max()).await;
            println!("{}", plan.note);
            (plan.tasks, req.task.clone())
        }
        FanOut::Auto => {
            let plan =
                plan_fanout(agent.clone(), req.task.clone(), None, config.agents_max()).await;
            println!("{}", plan.note);
            (plan.tasks, req.task.clone())
        }
    };

    if tasks.len() > 1 {
        for (i, t) in tasks.iter().enumerate() {
            println!("  {}. {t}", i + 1);
        }
    }
    let report = workers.spawn_many_on(tasks, build_worker_prompt(cwd, mem), request, over);
    println!("{}", fanout_notice(&report));
}

fn handle_agents<'a>(mut parts: impl Iterator<Item = &'a str>, workers: &mut WorkerManager) {
    match parts.next().unwrap_or("list") {
        "list" | "" => {
            let list = workers.list();
            if list.is_empty() && workers.queued_count() == 0 {
                println!("(no agents)");
            }
            for w in list {
                let nudges =
                    if w.nudges > 0 { format!(" · {} nudges", w.nudges) } else { String::new() };
                let on = w.model.as_deref().map(|m| format!(" · on {m}")).unwrap_or_default();
                println!(
                    "{} [{}] {} tools · {} changed{}{} — {}",
                    w.id,
                    w.status.label(),
                    w.tool_calls,
                    w.changed.len(),
                    nudges,
                    on,
                    w.task
                );
            }
            if workers.queued_count() > 0 {
                println!("({} queued)", workers.queued_count());
            }
        }
        "show" | "result" => match parts.next().and_then(|id| workers.get(id)) {
            Some(w) => {
                println!("{} [{}]", w.id, w.status.label());
                if let Some(reason) = &w.escalation {
                    println!("stopped by supervisor: {reason}");
                }
                if !w.changed.is_empty() {
                    println!("changed: {}", w.changed.join(", "));
                }
                println!("{}", if w.result.is_empty() { &w.last } else { &w.result });
            }
            None => println!("usage: /agents show <id>"),
        },
        "kill" | "stop" => match parts.next() {
            Some(id) if workers.kill(id) => println!("killing {id}"),
            Some(id) => println!("(no agent {id})"),
            None => println!("usage: /agents kill <id>"),
        },
        "nudge" | "steer" => {
            let id = parts.next().map(str::to_string);
            let msg = parts.collect::<Vec<_>>().join(" ");
            match id {
                Some(id) if !msg.trim().is_empty() => {
                    if workers.nudge(&id, &msg) {
                        println!("nudged {id}");
                    } else {
                        println!("(no agent {id})");
                    }
                }
                _ => println!("usage: /agents nudge <id> <message>"),
            }
        }
        "drop-queued" | "clear-queue" => {
            println!("dropped {} queued task(s)", workers.drop_queued())
        }
        other => eprintln!("unknown /agents subcommand: {other}"),
    }
}

/// `/skill [name]` — list installed skills, or print one's instructions.
fn handle_skill<'a>(mut parts: impl Iterator<Item = &'a str>, cwd: &Path) {
    let catalog = worksmith::skill::SkillCatalog::discover(cwd);
    match parts.next() {
        None => {
            if catalog.is_empty() {
                println!("(no skills — add one under .worksmith/skills/<name>/SKILL.md)");
            }
            for s in catalog.skills() {
                println!("{}: {}", s.name, s.description);
            }
            for note in catalog.notes() {
                println!("({note})");
            }
        }
        Some(name) => match catalog.get(name) {
            Some(skill) => match skill.body() {
                Ok(body) => {
                    println!("skill `{}` ({})\n\n{}", skill.name, skill.dir.display(), body.trim())
                }
                Err(e) => eprintln!("could not read `{name}`: {e}"),
            },
            None => eprintln!("no skill named `{name}`"),
        },
    }
}

/// `/knowledge [index | search <query> | status]`
fn handle_knowledge<'a>(mut parts: impl Iterator<Item = &'a str>, cwd: &Path) {
    let store = match worksmith::knowledge::KnowledgeStore::open(cwd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("knowledge unavailable: {e}");
            return;
        }
    };
    match parts.next().unwrap_or("status") {
        "index" | "reindex" => match store.index() {
            Ok(stats) => {
                let pruned = store.prune().unwrap_or(0);
                println!(
                    "indexed {} file(s) → {} chunk(s) · {} unchanged · {} stale removed",
                    stats.files, stats.chunks, stats.skipped_unchanged, pruned
                );
            }
            Err(e) => eprintln!("indexing failed: {e}"),
        },
        "search" | "find" => {
            let query = parts.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                println!("usage: /knowledge search <query>");
                return;
            }
            match store.search(&query, 5) {
                Ok(hits) if hits.is_empty() => println!("(no matches)"),
                Ok(hits) => {
                    for h in hits {
                        println!("--- {} (chunk {})\n{}\n", h.source, h.ord, h.text);
                    }
                }
                Err(e) => eprintln!("knowledge search failed: {e}"),
            }
        }
        "status" | "" => match store.chunk_count() {
            Ok(n) => println!("knowledge index: {n} chunk(s)"),
            Err(e) => eprintln!("knowledge error: {e}"),
        },
        other => eprintln!("unknown /knowledge subcommand: {other}"),
    }
}

/// Resolve a user-typed (usually abbreviated) memory id, reporting the failure
/// itself. Same rule as the TUI: any unambiguous prefix works.
fn resolve_memory_id(mem: &MemoryStore, typed: &str) -> Option<String> {
    use worksmith::memory::{IdMatch, short_id};
    match mem.resolve_id(typed) {
        Ok(IdMatch::Unique(id)) => Some(id),
        Ok(IdMatch::None) => {
            println!("(no memory matching `{typed}`)");
            None
        }
        Ok(IdMatch::Ambiguous(ids)) => {
            let shown: Vec<&str> = ids.iter().map(|i| short_id(i)).collect();
            println!("`{typed}` matches {}: {}", ids.len(), shown.join(", "));
            None
        }
        Err(e) => {
            eprintln!("error: {e}");
            None
        }
    }
}

async fn handle_memory<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    mem: &MemoryStore,
    agent: &Arc<Agent>,
    session: &Session,
) {
    let sub = parts.next().unwrap_or("list");
    match sub {
        "mine" => {
            let limit = parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(10);
            let cwd = Path::new(session.cwd());
            let plan = match worksmith::mining::plan(mem, cwd, limit) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("mine failed: {e:#}");
                    return;
                }
            };
            if plan.items.is_empty() {
                println!("{}", plan.report.summary());
                return;
            }
            let results = worksmith::mining::classify(agent, &plan.items, |i, n| {
                println!("({i}/{n}) mining…");
            })
            .await;
            let report = worksmith::mining::record(mem, results, plan.report);
            println!("{}", report.summary());
            for f in &report.failed {
                eprintln!("mine: {f}");
            }
        }
        "search" | "find" => {
            let query = parts.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                println!("usage: /memory search <query>");
                return;
            }
            match mem.search(&query, 10) {
                Ok(hits) if hits.is_empty() => println!("(nothing remembered about \"{query}\")"),
                Ok(hits) => {
                    for h in hits {
                        println!(
                            "{:.2}  {}  [{}/{}] {}: {}",
                            h.score, h.row.id, h.row.scope, h.row.kind, h.row.subject, h.row.content
                        );
                    }
                }
                Err(e) => eprintln!("memory error: {e}"),
            }
        }
        "pending" | "proposed" => match mem.pending() {
            Ok(rows) if rows.is_empty() => println!("(no proposals from workers)"),
            Ok(rows) => {
                for r in rows {
                    println!(
                        "{}  [{}/{}] {}: {}  (/memory approve {} | /memory forget {})",
                        r.id, r.scope, r.kind, r.subject, r.content, r.id, r.id
                    );
                }
            }
            Err(e) => eprintln!("memory error: {e}"),
        },
        "approve" => match parts.next() {
            Some("all") => match mem.pending_ids() {
                Ok(ids) if ids.is_empty() => println!("(nothing pending)"),
                Ok(ids) => {
                    let n = ids.iter().filter(|id| mem.approve(id).unwrap_or(false)).count();
                    println!("approved {n} proposals");
                }
                Err(e) => eprintln!("memory error: {e}"),
            },
            Some(t) => {
                let Some(id) = resolve_memory_id(mem, t) else { return };
                match mem.approve(&id) {
                    Ok(true) => println!("approved {}", worksmith::memory::short_id(&id)),
                    Ok(false) => println!("(no pending proposal {t})"),
                    Err(e) => eprintln!("memory error: {e}"),
                }
            }
            None => println!("usage: /memory approve <id|all>"),
        },
        "extract" | "distill" => {
            let transcript = render_recent(session, 40);
            if transcript.trim().is_empty() {
                println!("(nothing to distill yet)");
                return;
            }
            println!("(distilling…)");
            let text = match agent
                .ask(worksmith::memory::EXTRACTION_PROMPT, &transcript, 512)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("extraction failed: {e:#}");
                    return;
                }
            };
            let candidates = worksmith::memory::parse_candidates(&text);
            if candidates.is_empty() {
                println!("nothing worth remembering");
            }
            for c in candidates {
                match mem.remember_deduped(c.scope, &c.kind, &c.subject, &c.content, c.importance) {
                    Ok((row, true)) => println!(
                        "remembered {} [{}/{}] {}: {}",
                        row.id, row.scope, row.kind, row.subject, row.content
                    ),
                    Ok((row, false)) => {
                        println!("already known: {}: {}", row.subject, row.content)
                    }
                    Err(e) => eprintln!("memory error: {e}"),
                }
            }
        }
        "list" | "" => print_memories(mem, None),
        "global" => print_memories(mem, Some(Scope::Global)),
        "project" => print_memories(mem, Some(Scope::Project)),
        "show" => {
            let Some(id) = parts.next() else {
                eprintln!("usage: /memory show <id>");
                return;
            };
            let Some(id) = resolve_memory_id(mem, id) else { return };
            match mem.get(&id) {
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
            let Some(id) = resolve_memory_id(mem, id) else { return };
            match mem.forget(&id) {
                Ok(true) => println!("(forgot {})", worksmith::memory::short_id(&id)),
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

/// `--fast` / `--think [budget]` beat the configured default. `--think` with no
/// value means plain on; with a number it caps the reasoning alone, leaving the
/// rest of `max-tokens` for an answer.
fn resolve_thinking(
    args: &Args,
    config: &Config,
) -> anyhow::Result<Option<worksmith::llm::Thinking>> {
    use worksmith::llm::Thinking;
    if args.fast {
        return Ok(Some(Thinking::Off));
    }
    let Some(v) = args.think.as_deref().map(str::trim) else {
        return Ok(config.thinking());
    };
    if v.eq_ignore_ascii_case("on") {
        return Ok(Some(Thinking::On));
    }
    if v.eq_ignore_ascii_case("off") {
        return Ok(Some(Thinking::Off));
    }
    if let Some(e) = worksmith::llm::Effort::parse(v) {
        return Ok(Some(Thinking::Effort(e)));
    }
    let budget = v
        .strip_suffix(['k', 'K'])
        .map(|h| h.trim().parse::<f32>().ok().map(|f| (f * 1000.0) as u32))
        .unwrap_or_else(|| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--think expects on, off, an effort (minimal|low|medium|high), \
                 or a token budget (got `{v}`)"
            )
        })?;
    Ok(Some(Thinking::Budget(budget)))
}

/// Ask about a project's own config the first time worksmith sees it.
///
/// A project config can run shell commands (`agent.validate`) and redirect model
/// traffic (`providers.*.base-url`), so it is not applied just because you `cd`'d
/// into the repo. This runs before the TUI exists, on plain stdin — once per
/// project, and again if the file changes.
fn resolve_project_trust(config: Config, cwd: &Path, args: &Args) -> Result<Config> {
    use std::io::Write;
    use worksmith::trust::{Decision, TrustStore};

    let Some(prompt) = config.pending_trust.clone() else {
        return Ok(config); // no project config, or already decided
    };

    // Explicitly pre-decided.
    if args.trust_project {
        let mut store = TrustStore::load();
        store.record(cwd, &prompt.fingerprint, Decision::Trust);
        return Config::load_trusted(cwd);
    }

    // Nobody to ask: skip the project config and say so. Failing the run would
    // be worse — the global config is a perfectly good way to work.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "note: ignoring {} — this project's config has not been trusted.\n\
             Run worksmith here interactively to decide, or pass --trust-project.",
            prompt.config_path.display()
        );
        return Ok(config);
    }

    if prompt.changed_since_trusted {
        println!("\nThis project's worksmith config has CHANGED since you trusted it:");
    } else {
        println!("\nThis project has its own worksmith config:");
    }
    println!("  {}", prompt.config_path.display());
    println!();
    for (key, value, consequence) in &prompt.settings {
        match consequence {
            Some(why) => println!("  ! {key} = {value}\n      {why}"),
            None => println!("    {key} = {value}"),
        }
    }
    println!();

    loop {
        print!("Use it? [y] yes  [n] no, ignore it  [v] view the file: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Ok(config); // treat an unreadable answer as "no"
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => {
                let mut store = TrustStore::load();
                store.record(cwd, &prompt.fingerprint, Decision::Trust);
                return Config::load_trusted(cwd);
            }
            "n" | "no" | "" => {
                let mut store = TrustStore::load();
                store.record(cwd, &prompt.fingerprint, Decision::Ignore);
                println!("Ignoring it. Using your global config for this project.");
                return Ok(config);
            }
            "v" | "view" => match std::fs::read_to_string(&prompt.config_path) {
                Ok(body) => println!("\n----- {} -----\n{body}-----", prompt.config_path.display()),
                Err(e) => println!("(could not read it: {e})"),
            },
            other => println!("(didn't understand `{other}`)"),
        }
    }
}
