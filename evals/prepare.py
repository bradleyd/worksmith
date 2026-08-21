"""Unload whatever oMLX is holding, then decide whether a model will fit.

Exits non-zero when it will not, which stops the phase. The arithmetic is in
omlx.py so it can be tested; this is the part that talks to the machine.
"""

import os
import subprocess
import sys

import omlx

base = os.environ["OMLX"]
model = os.environ["MODEL"]
fallback = float(os.environ["FALLBACK"])

cookie = omlx.login(
    base, [os.environ.get("OMLX_ADMIN_KEY", ""), os.environ.get("OMLX_API_KEY", "")]
)
print("admin session established" if cookie else "admin login failed", file=sys.stderr)
if not cookie:
    print("cannot unload for you; unload in the oMLX app between phases", file=sys.stderr)

for m in (os.environ["LOCAL_SMALL"], os.environ["LOCAL_BIG"]):
    _, msg = omlx.unload(base, cookie, m)
    print(f"  {m}: {msg}")

still_loaded = omlx.loaded_models(base, cookie)
if still_loaded:
    # The server names them; RSS only ever said how much, never which.
    print(f"note: oMLX still has loaded: {', '.join(still_loaded)}")

need = omlx.required_gb(omlx.model_size_gb(base, cookie, model), fallback)
have = omlx.free_gb()
print(f"free memory: {have:.1f} GB (need ~{need} GB for {model})")

if have < need:
    print(f"REFUSING: not enough free memory for {model}.", file=sys.stderr)
    print("Biggest processes right now:", file=sys.stderr)
    ps = subprocess.run(["ps", "-Ao", "rss,comm"], capture_output=True, text=True).stdout
    rows = []
    for line in ps.splitlines():
        parts = line.split(None, 1)
        if len(parts) == 2 and parts[0].isdigit() and int(parts[0]) > 400_000:
            rows.append((int(parts[0]) / 1048576, parts[1]))
    for gb, name in sorted(rows, reverse=True)[:6]:
        print(f"  {gb:.1f} GB  {name}", file=sys.stderr)
    sys.exit(1)
