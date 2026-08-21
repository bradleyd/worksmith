#!/usr/bin/env bash
# Two questions, run in the only order that fits in memory.
#
#   Phase 1 (regression): hosted 27B drives the loop, the local 9B does the
#                         worker jobs. Only the 9B is resident: ~6.7 GB.
#   Phase 2 (daily driver): the local 27B does everything. ~17 GB resident.
#
# oMLX has `idle_timeout_seconds: null`, so it never unloads a model on its own.
# Run these back to back without unloading and you hold both sets of weights
# plus KV cache on a 36 GB machine, which is the exhaustion this guards against.
#
# The memory arithmetic and admin calls live in omlx.py, where they are tested
# (`python3 -m unittest omlx_test`). This file is the running order and nothing
# else. It collected three bugs in an afternoon while it did the thinking
# itself, and `bash -n` caught none of them.
#
# Needs OMLX_API_KEY. Unloading also needs a credential the admin UI accepts;
# export OMLX_ADMIN_KEY if OMLX_API_KEY is not it.
set -uo pipefail

cd "$(dirname "$0")"
export OMLX=${OMLX:-http://127.0.0.1:8000}
HOSTED=${HOSTED:-openrouter/qwen/qwen3.8-27b}
export LOCAL_SMALL=${LOCAL_SMALL:-Qwen3.5-9B-OptiQ-4bit}
export LOCAL_BIG=${LOCAL_BIG:-Qwen3.8-27B-OptiQ-4bit}
REPEAT=${REPEAT:-1}
OUT=${OUT:-$PWD/results}
mkdir -p "$OUT"

: "${OMLX_API_KEY:?export OMLX_API_KEY first (oMLX requires it, even for /v1/models)}"

banner() { printf '\n=== %s ===\n' "$1"; }

# prepare <model> <fallback_gb>: unload everything, then refuse if the model
# will not fit. Non-zero exit means do not start this phase.
prepare() { MODEL="$1" FALLBACK="$2" python3 prepare.py; }

banner "binary"
command -v worksmith >/dev/null && worksmith --version
which -a worksmith

banner "phase 1: hosted loop + local 9B workers"
prepare "$LOCAL_SMALL" 7 || exit 1
# The main loop, hosted: does the harness still drive a turn end to end?
python3 run.py --model "$HOSTED" --modes raw,guided --repeat "$REPEAT" \
  --json "$OUT/p1-hosted-loop.json"
# The worker path, drafting on the small local model. Only newsletter-judge
# declares `workers`, so this is one task by design.
python3 run.py --model "$HOSTED" --worker-model "omlx/$LOCAL_SMALL" \
  --modes workers --repeat "$REPEAT" --json "$OUT/p1-local-workers.json"

banner "phase 2: local 27B for everything"
prepare "$LOCAL_BIG" 19 || exit 1
python3 run.py --model "omlx/$LOCAL_BIG" --modes raw,guided --repeat "$REPEAT" \
  --json "$OUT/p2-local-27b.json"

banner "results"
ls -1 "$OUT"/*.json
echo "compare tokens-per-solved between p1-hosted-loop and p2-local-27b:"
echo "  that is the daily-driver question, 4-bit local against the hosted build."
echo "which model actually answered:"
echo "  grep -ho '\"model\":\"[^\"]*\"' ~/.worksmith/sessions/*.jsonl | sort | uniq -c"
