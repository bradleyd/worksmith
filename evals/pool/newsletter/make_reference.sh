#!/usr/bin/env bash
# Materialise a correct solution, so verify.py can prove every check in every
# newsletter backlog is satisfiable before a model is ever pointed at them.
set -euo pipefail
d="$1"; mkdir -p "$d/ideas" "$d/drafts"
cat > "$d/ideas/topics.md" <<'EOF'
# Topics
- Measuring quantisation damage on small local models
- Whether a validation loop beats a bigger model
- Prompt caching and the real cost of a fresh context per task
EOF
cat > "$d/drafts/article1.md" <<'EOF'
# Testing Local AI Models Honestly

Running a model on your own machine changes which questions are worth asking.
Throughput stops being the interesting number and reliability takes over, because
nobody is waiting on a queue and nothing is billed by the token. What you want to
know instead is whether the same prompt gives the same answer twice, and whether
a smaller model held to a strict check can stand in for a larger one left to its
own judgement. Those are measurable things, and measuring them turns out to be
harder than running the models.

## Background

Local inference on consumer hardware is bounded by memory bandwidth rather than
compute, so the headline tokens per second is close to a property of the machine
rather than of the software. That makes it a poor figure to tune against. The
numbers that move are elsewhere: how much of a prompt is served from cache, how
often a model has to be told again, and how many attempts a task takes before a
check agrees it is finished. None of those appear on a dashboard by default, and
all of them dominate the experience of actually working with a small model for
an afternoon rather than benchmarking it for a minute.

## What we tried

We ran the same task list against several model sizes, first inside a harness
that retries against a check and then bare, with no tools and a single attempt.
The bare arm mattered more than expected: it separated what the model can do
from what the loop recovers, and it ran in minutes where the harness runs took
hours. It also caught two of our own bugs, because a task that fails for a
harness reason looks exactly like a task the model got wrong unless you are
recording enough to tell them apart. That distinction turned out to be the whole
experiment, and we had been discarding the evidence for it.
EOF
python3 - "$d" <<'PY'
import json, sys, pathlib
d = pathlib.Path(sys.argv[1])
body = (d / "drafts/article1.md").read_text()
title = body.splitlines()[0].lstrip("# ").strip()
(d / "drafts/article1.meta.json").write_text(json.dumps(
    {"title": title, "status": "draft", "word_count": len(body.split())}, indent=2))
(d / "README.md").write_text("# Drafts\n\n- drafts/article1.md\n")
(d / "Makefile").write_text("check:\n\twc -w drafts/article1.md\n")
PY
