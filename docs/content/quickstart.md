+++
title = "Quickstart"
description = "Install Worksmith, connect a model, and run a task in the TUI or from the command line."
weight = 10
+++

You need a terminal, a local or hosted model that supports tool calling through
an OpenAI-compatible API, and a repository to work in.

[Install](#install) · [Connect a model](#connect-a-model) ·
[Use the TUI](#use-the-tui) · [Run a command](#run-a-command)

## Install

Choose one installation method.

**Homebrew (macOS):**

```sh
brew tap bradleyd/worksmith
brew install bradleyd/worksmith/worksmith
```

**Prebuilt binary:** download the archive for your platform from
[GitHub releases](https://github.com/bradleyd/worksmith/releases), extract it,
and put `worksmith` on your PATH.

**From source (requires Rust):**

```sh
git clone https://github.com/bradleyd/worksmith
cd worksmith
./install.sh
```

Check the installation:

```sh
worksmith --version
```

## Connect a model

Run `worksmith` once. On first launch it creates `~/.worksmith/` and an
annotated `config.example.toml`. Copy that file, then open the copy in your editor:

```sh
cp ~/.worksmith/config.example.toml ~/.worksmith/config.toml
```

For OpenRouter, the following is a complete minimal `config.toml`. Set `model`
to the ID of a tool-capable model available to your account, prefixed with
`openrouter/`.

```toml
model = "openrouter/qwen/qwen3.8-27b"
max-tokens = 8192

[providers.openrouter]
base-url = "https://openrouter.ai/api/v1"
api-key-env = "OPENROUTER_API_KEY"
```

Set your key in the same terminal where you will run Worksmith:

```sh
export OPENROUTER_API_KEY="your-api-key"
```

Using a local server or another provider? See [model configuration](@/guide/configuration.md)
for local model setup, OpenAI, vLLM, and oMLX examples. Once configured, either
mode below uses the same model and tools.

## Use the TUI

The full-screen terminal interface is the default. Open a terminal in the
repository you want to work on, then launch:

```sh
worksmith
```

<figure class="screenshot">
  <a href="/images/worksmith-tui-commands.png" aria-label="View the Worksmith TUI screenshot at full size">
    <img src="/images/worksmith-tui-commands.png" width="2624" height="1696" alt="Worksmith TUI with the command menu open, showing help, skills, workers, validation, and metrics above the message input and model status bar." loading="lazy" decoding="async">
  </a>
  <figcaption>The TUI with its command menu open. Type a message in the input area at the bottom. Select the image to view it at full size.</figcaption>
</figure>

Type a task in the input area and press **Enter**, for example:

```text
Explain how this project is organized and where its tests live.
```

The transcript shows the response and tool activity. You can ask follow-up
questions in the same session.

To work on a failing test, set the check first by entering:

```text
/validate cargo test
```

Then send your task:

```text
Make the failing test pass.
```

Replace `cargo test` with your project's test command. Worksmith runs that
check before accepting completion and sends failures back to the model to retry.
Use `/validate off` to clear the check for later tasks.

You can also launch the TUI with the check and first task already supplied:

```sh
worksmith --until "cargo test" "make the failing test pass"
```

| Action | Key or command |
| --- | --- |
| Send a message | Enter |
| Add a newline | Ctrl+N |
| Expand tool output | Ctrl+O |
| Stop the current turn | Esc |
| Read the transcript | Esc with an empty input; `i` to return to typing |
| Show available commands | `/help` |
| Explain the status footer | `/help footer` |
| Exit | `/quit` or Ctrl+C |

## Run a command

Use `--print` for a single task: Worksmith runs it, prints the final answer,
and exits. This is useful from a shell or script.

```sh
worksmith --print "summarize src/main.rs"
```

To require a passing check for that task:

```sh
worksmith --print --until "cargo test" "make the failing test pass"
```

A prompt alone opens the TUI in an interactive terminal. Add `--print` when
you want a one-shot run. Both modes can read files, edit code, and run tools.

For an interactive session without the full-screen interface:

```sh
worksmith --plain
```

For scripts that consume structured events:

```sh
worksmith --mode json "list the rust files"
```

One-shot runs cannot ask you to approve a tool action. If an action needs
approval, run the task interactively so you can review it.

## Understand the result

With `--until` or `/validate`, a task is complete only when your check exits
successfully. Failed checks trigger a bounded number of retries; Worksmith
can still stop with a failure if it cannot resolve the problem. A passing
check verifies what that command covers, so review the changes as well.

- [Validation loop](@/guide/validation-loop.md): how checks and retries work.
- [Model configuration](@/guide/configuration.md): provider examples and setup fixes.
- [Metrics and cost](@/guide/metrics.md): understand usage and the status footer.
