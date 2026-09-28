# Scope of the abnormal-exit fix

## Question

The terminal-restore fix (decision a) is done. But the same `kill`/panic path also orphans the MCP coprocess: it runs in its own process group and is only reaped by Drop/shutdown, which `process::exit` skips. That orphaning already happens today; my fix just doesn't address it.

Two options:

(a) Terminal-only. Ship the terminal-restore fix as scoped. The MCP coprocess-orphan gap stays open (pre-existing) and gets its own ticket. Smallest change, exactly what you approved.

(b) Terminal + reap MCP coprocesses. Register each MCP coprocess's process-group id in a small process-global registry when a connection starts (and drop it on close), and have the signal/panic handler `kill(-pgid, SIGKILL)` every registered group before exiting. Closes the whole "abnormal termination leaves things behind" class in one change. Costs: a `Mutex<HashSet<i32>>`-style global in the MCP module, plus a couple of lines at connection start/close. Safe because at signal time the process is dying anyway.

I recommend (b): it's the same bug family, the MCP side is small, and leaving a coprocess running is exactly the kind of thing the kill_group machinery exists to prevent. Which do you want?

## Decision

lets do b
