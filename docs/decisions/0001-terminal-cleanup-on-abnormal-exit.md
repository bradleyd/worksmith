# Terminal cleanup on abnormal exit

## Question

The incident exposed that worksmith only restores the terminal on a clean exit (tui.rs:651) and the Ctrl+G editor path (tui.rs:1318). A SIGTERM (`kill`), a panic, or an OOM kill skips restore_terminal, leaving the terminal in raw mode with mouse capture on — the gibberish you hit.

How do you want to handle the fix?

(a) Add a SIGTERM + SIGINT process-level handler (e.g. signal_hook/ctrlc) that runs the same restore_terminal as a clean exit, plus a panic::set_hook. Covers `kill` and stray Ctrl+C and panics. Still can't catch `kill -9`/OOM. This is the targeted, low-risk fix.

(b) (a) plus revisit the mouse-capture default — e.g. document the risk more prominently or make the restore more defensive. Bigger, touches the deliberate mouse-capture decision documented at tui.rs:660.

(c) Just record it as a loose end for now and don't change code yet.

I recommend (a). Which do you want?

## Decision

a
