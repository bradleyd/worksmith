# Scope for the abnormal-exit follow-up

## Question

The signal-handling code is done and correct; the session's "it hangs" diagnosis was wrong. Where should I focus next?

A) Validate + commit only — PTY-test the SIGTERM/panic restore, fix the thread-id concern if real, then commit the signal work. Smallest, gets the fix landed.

B) A) + the deferred MCP-orphaning ticket — also fix coprocesses surviving process::exit on kill/panic. Bigger; touches process-group cleanup.

C) Just the review — you'll drive the rest yourself.

My recommendation is A: land the fix that's already written and verified-green, and keep the MCP ticket separate as originally intended.

## Decision

a
