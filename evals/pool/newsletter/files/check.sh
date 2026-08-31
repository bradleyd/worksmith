#!/usr/bin/env bash
# End-to-end check for the newsletter fixture. IDENTICAL across granularities.
#
# It looks only at the artifacts on disk — directories, headings, word counts,
# valid JSON — so it cannot tell how the work was divided, which is what lets it
# grade coarse and fine as the same job. Nothing here needs a model to judge it.
set -uo pipefail
fail=0
say() { echo "FAIL: $1"; fail=1; }

[ -d ideas ]   || say "no ideas/ directory"
[ -d drafts ]  || say "no drafts/ directory"

head -1 ideas/topics.md 2>/dev/null | grep -qx '# Topics' || say "ideas/topics.md does not start with '# Topics'"
n=$(grep -c '^- ' ideas/topics.md 2>/dev/null || echo 0)
[ "$n" -eq 3 ] || say "ideas/topics.md has $n bullets, want 3"

A=drafts/article1.md
head -1 "$A" 2>/dev/null | grep -Eq '^# +\S+( +\S+){2,}' || say "$A has no 3+ word '# ' title"
grep -qx '## Background' "$A" 2>/dev/null || say "$A has no '## Background'"
grep -qx '## What we tried' "$A" 2>/dev/null || say "$A has no '## What we tried'"

bg=$(sed -n '/^## Background/,/^## What we tried/p' "$A" 2>/dev/null | sed '1d;$d' | wc -w)
[ "$bg" -ge 80 ] || say "Background has $bg words, want 80+"
wt=$(sed -n '/^## What we tried/,$p' "$A" 2>/dev/null | tail -n +2 | wc -w)
[ "$wt" -ge 80 ] || say "What we tried has $wt words, want 80+"

python3 - <<'PY' || fail=1
import json, sys
try:
    d = json.load(open("drafts/article1.meta.json"))
except Exception as e:
    print(f"FAIL: article1.meta.json unreadable: {e}"); sys.exit(1)
ok = True
if d.get("status") != "draft":
    print(f"FAIL: status is {d.get('status')!r}, want 'draft'"); ok = False
if len(str(d.get("title", "")).split()) < 3:
    print(f"FAIL: title {d.get('title')!r} is under three words"); ok = False
actual = len(open("drafts/article1.md").read().split())
if d.get("word_count") != actual:
    print(f"FAIL: word_count {d.get('word_count')!r}, file has {actual}"); ok = False
sys.exit(0 if ok else 1)
PY

grep -q '^- .*article1\.md' README.md 2>/dev/null || say "README.md does not list drafts/article1.md as a bullet"
grep -qE '^check:' Makefile 2>/dev/null || say "Makefile has no check target"
make -n check >/dev/null 2>&1 || say "make check is not runnable"

[ "$fail" -eq 0 ] && echo ok
exit $fail
