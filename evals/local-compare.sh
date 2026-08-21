#!/usr/bin/env bash
# Two questions, run in the only order that fits in memory.
#
#   Phase 1 (regression): hosted 27B drives the loop, the local 9B does the
#                         worker jobs. Only the 9B is resident: ~7.7 GB.
#   Phase 2 (daily driver): the local 27B does everything. ~19 GB resident.
#
# oMLX has `idle_timeout_seconds: null`, so it never unloads a model on its
# own. Run these back to back without unloading and you hold ~27 GB of weights
# plus KV cache on a 36 GB machine, which is the exhaustion this guards against.
# Every phase therefore unloads first and checks free memory before starting.
#
# Needs OMLX_API_KEY in the environment.
set -uo pipefail

OMLX=${OMLX:-http://127.0.0.1:8000}
HOSTED=${HOSTED:-openrouter/qwen/qwen3.8-27b}
LOCAL_SMALL=${LOCAL_SMALL:-Qwen3.5-9B-OptiQ-4bit}
LOCAL_BIG=${LOCAL_BIG:-Qwen3.8-27B-OptiQ-4bit}
REPEAT=${REPEAT:-1}
OUT=${OUT:-$(cd "$(dirname "$0")" && pwd)/results}
cd "$(dirname "$0")"
mkdir -p "$OUT"

: "${OMLX_API_KEY:?export OMLX_API_KEY first (oMLX requires it, even for /v1/models)}"
auth=(-H "Authorization: Bearer $OMLX_API_KEY")

# /admin/api/* answers "Admin authentication required" to the inference key.
# oMLX keeps two credentials in ~/.oMLX/settings.json: auth.api_key (inference)
# and auth.secret_key. Export the latter as OMLX_ADMIN_KEY to let this script
# unload models for you:
#
#   export OMLX_ADMIN_KEY="$(python3 -c "import json,pathlib; \
#     print(json.loads((pathlib.Path.home()/'.oMLX/settings.json').read_text())['auth']['secret_key'])")"
#
# Without it the phases still run, but you have to unload in the oMLX app
# between them, and this script will stop and say so rather than let the second
# model load on top of the first.
# Admin auth is a *login*, not a bearer token: POST /admin/api/login with
# {"api_key": …} sets a session cookie, and /admin/api/* wants that cookie.
# Sending either key as a Bearer just gets "Admin authentication required".
COOKIES=$(mktemp -t omlx-admin)
trap 'rm -f "$COOKIES"' EXIT

admin_login() {
  # Which credential the login wants is not documented, and this machine has
  # two (auth.api_key and auth.secret_key), so try both and say which worked.
  local key label
  for key in "${OMLX_ADMIN_KEY:-}" "${OMLX_API_KEY:-}"; do
    [ -z "$key" ] && continue
    label="${#key}-char key"
    if curl -s -m 10 -c "$COOKIES" -o /dev/null -X POST \
         -H 'Content-Type: application/json' \
         -d "{\"api_key\":\"$key\",\"remember\":true}" \
         "$OMLX/admin/api/login" &&
       [ "$(curl -s -m 10 -b "$COOKIES" -o /dev/null -w '%{http_code}' \
            "$OMLX/admin/api/models")" = "200" ]; then
      echo "admin session established ($label)"
      return 0
    fi
  done
  echo "admin login failed: cannot unload models automatically." >&2
  echo "Unload in the oMLX app between phases, or set OMLX_ADMIN_KEY to the" >&2
  echo "credential its admin UI accepts (auth.* in ~/.oMLX/settings.json)." >&2
  return 1
}

free_gb() {
  # Pages free + inactive + speculative, which is what is actually available.
  vm_stat | awk '
    /page size of/ { ps = $8 }
    /Pages free/ { f = $3 }
    /Pages inactive/ { i = $3 }
    /Pages speculative/ { s = $3 }
    END { gsub(/\./, "", f); gsub(/\./, "", i); gsub(/\./, "", s);
          printf "%.1f", (f + i + s) * ps / 1073741824 }'
}

loaded() { curl -s -m 10 "${auth[@]}" "$OMLX/admin/api/stats" 2>/dev/null; }

unload_all() {
  # Report what happened. The first version swallowed output, and phase 2 then
  # refused with the 9B still holding 7.2 GB — the unload had failed and said
  # nothing, which is the failure mode this whole codebase keeps hitting.
  for m in "$LOCAL_SMALL" "$LOCAL_BIG"; do
    local code body
    body=$(curl -s -m 30 -w '\n%{http_code}' -X POST -b "$COOKIES" \
      "$OMLX/admin/api/models/$(printf %s "$m" | sed 's|/|%2F|g')/unload" 2>&1)
    code=${body##*$'\n'}
    case "$code" in
      2*) echo "unloaded $m" ;;
      # "Model not loaded" is the answer when there is nothing to free, which is
      # the normal case at the start of a phase. Not a failure.
      400|404) case "$body" in
                 *"not loaded"*) echo "$m: already free" ;;
                 *) echo "unload $m: HTTP $code ${body%$'\n'*}" ;;
               esac ;;
      401) echo "unload $m: 401 — the admin session was not accepted" ;;
      *)   echo "unload $m: HTTP $code ${body%$'\n'*}" ;;
    esac
  done
  sleep 2
}

# Is oMLX holding a model right now? Cheaper and more direct than inferring it
# from free memory, and it is the thing that breaks phase 2.
omlx_rss_gb() {
  ps -Ao rss,comm | awk '/omlx-server/ { printf "%.1f", $1/1048576; exit }'
}

require_free() { # require_free <gb> <what>
  local need=$1 what=$2 have resident
  resident=$(omlx_rss_gb)
  if [ -n "$resident" ] && awk -v r="$resident" 'BEGIN { exit !(r > 3) }'; then
    echo "note: oMLX is holding ~${resident} GB (a model is still loaded)"
  fi
  have=$(free_gb)
  echo "free memory: ${have} GB (need ~${need} GB for ${what})"
  if awk -v h="$have" -v n="$need" 'BEGIN { exit !(h < n) }'; then
    echo "REFUSING: not enough free memory for ${what}." >&2
    echo "Biggest processes right now:" >&2
    ps -Ao rss,comm | awk '$1 > 400000 { printf "  %.1f GB  %s\n", $1/1048576, $2 }' \
      | sort -rn | head -6 >&2
    echo "Close some of those and re-run, or run this phase on its own." >&2
    return 1
  fi
}

banner() { printf '\n=== %s ===\n' "$1"; }

banner "binary"
# The stale-binary trap: a config newer than the binary fails confusingly.
command -v worksmith >/dev/null && worksmith --version
which -a worksmith

banner "admin session"
if ! admin_login; then
  echo "continuing without automatic unloads; watch memory yourself." >&2
fi

banner "phase 1: hosted loop + local 9B workers"
unload_all
require_free 10 "the 9B" || exit 1
# The main loop, hosted. This is the regression check: does the harness still
# drive a turn end to end after a day of changes?
python3 run.py --model "$HOSTED" --modes raw,guided --repeat "$REPEAT" \
  --json "$OUT/p1-hosted-loop.json"
# The worker path, with the small local model doing the drafting. Only
# 08-newsletter-judge declares `workers`, so this is one task by design.
python3 run.py --model "$HOSTED" --worker-model "omlx/$LOCAL_SMALL" \
  --modes workers --repeat "$REPEAT" --json "$OUT/p1-local-workers.json"

banner "phase 2: local 27B for everything"
unload_all
require_free 21 "the 27B" || exit 1
python3 run.py --model "omlx/$LOCAL_BIG" --modes raw,guided --repeat "$REPEAT" \
  --json "$OUT/p2-local-27b.json"

banner "results"
ls -1 "$OUT"/*.json
echo "compare pass rates and tokens-per-solved between p1-hosted-loop and p2-local-27b:"
echo "  that is the daily-driver question — 4-bit local against the hosted build."
