"""Show what /admin/api/models actually returns, so the size parser can stop
guessing. Prints the response shape, not the values."""

import json
import os
import sys

import omlx

base = os.environ.get("OMLX", "http://127.0.0.1:8000")
cookie = omlx.login(base, [os.environ.get("OMLX_ADMIN_KEY", ""), os.environ.get("OMLX_API_KEY", "")])
print("login:", "ok" if cookie else "FAILED (that alone would explain a None)")

status, body, _ = omlx._request(f"{base}/admin/api/models", cookie)
print("GET /admin/api/models →", status)
if status != 200:
    print(body[:300])
    sys.exit(1)

payload = json.loads(body)
items = payload if isinstance(payload, list) else (payload.get("models") or payload.get("data") or [])
print("entries:", len(items))
if items:
    first = items[0]
    print("keys on the first entry:", sorted(first) if isinstance(first, dict) else type(first))
    print("anything size-ish:", {
        k: v for k, v in first.items()
        if isinstance(k, str) and any(t in k.lower() for t in ("size", "mem", "byte", "gb"))
    } if isinstance(first, dict) else None)
