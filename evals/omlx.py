"""Memory arithmetic and oMLX admin calls for the local comparison runs.

This lived in shell and collected three bugs in an afternoon: an unload whose
error went to /dev/null, bearer auth against an endpoint that wants a login
session, and a hand-picked memory threshold that refused a run which would have
fit. `bash -n` catches none of those. Here they are testable, and the parts that
decide whether a 19 GB model is allowed to load are pure functions with no I/O.

Run the tests with:  python3 -m unittest evals.omlx_test
"""

from __future__ import annotations

import json
import subprocess
import urllib.error
import urllib.request

GIB = 1073741824

# Headroom over a model's reported size, for the KV cache and everything else
# that grows once generation starts.
KV_HEADROOM = 1.2

# oMLX holding less than this is not holding a model.
RESIDENT_MODEL_GB = 3.0


def parse_free_gb(vm_stat_output: str) -> float:
    """Available memory in GB, from `vm_stat`.

    Free pages alone are misleading on macOS (0.5 GB free on a machine with
    15 GB available), and `memory_pressure`'s "free percentage" is misleading
    the other way — it read 86% when 15.6 GB was actually reclaimable. Inactive
    and speculative pages are evictable, so they count.
    """
    page_size = 4096
    counts: dict[str, int] = {}
    for line in vm_stat_output.splitlines():
        if "page size of" in line:
            for word in line.split():
                if word.isdigit():
                    page_size = int(word)
                    break
        for key in ("free", "inactive", "speculative"):
            if line.startswith(f"Pages {key}"):
                counts[key] = int(line.split(":")[1].strip().rstrip("."))
    pages = sum(counts.get(k, 0) for k in ("free", "inactive", "speculative"))
    return pages * page_size / GIB


def parse_model_size_gb(models_json: str, model_id: str) -> float | None:
    """What oMLX says a model needs, in GB, or None if it cannot be found.

    Disk size overstates it: the 9B is 7.7 GB on disk and loads at 6.70 GB
    actual. The admin API's shape is not documented here, so several plausible
    keys are tried and anything implausibly large is read as bytes.
    """
    try:
        payload = json.loads(models_json)
    except (json.JSONDecodeError, TypeError):
        return None

    items = payload
    if isinstance(payload, dict):
        for key in ("models", "data", "items"):
            if isinstance(payload.get(key), list):
                items = payload[key]
                break
    if not isinstance(items, list):
        return None

    for entry in items:
        if not isinstance(entry, dict):
            continue
        if entry.get("id") != model_id and entry.get("name") != model_id:
            continue
        for key in ("size_gb", "memory_gb", "estimated_memory_gb", "size", "size_bytes"):
            value = entry.get(key)
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                continue
            if value <= 0:
                continue
            # A "size" of 19 is gigabytes; 20401094656 is bytes.
            return value / GIB if value > 10_000 else float(value)
    return None


def required_gb(size_gb: float | None, fallback_gb: float) -> float:
    """How much free memory a load needs, with headroom."""
    base = size_gb if size_gb and size_gb > 0 else fallback_gb
    return round(base * KV_HEADROOM, 1)


def parse_resident_gb(ps_output: str, needle: str = "omlx-server") -> float:
    """oMLX's resident size in GB, from `ps -Ao rss,comm`.

    Free memory is an inference; a model still being held is the fact, and it is
    what breaks the second phase.
    """
    for line in ps_output.splitlines():
        parts = line.split(None, 1)
        if len(parts) != 2 or not parts[0].isdigit():
            continue
        if needle in parts[1]:
            return int(parts[0]) / 1048576
    return 0.0


def holds_a_model(resident_gb: float) -> bool:
    return resident_gb > RESIDENT_MODEL_GB


def classify_unload(status: int, body: str) -> tuple[bool, str]:
    """Turn an unload response into (ok, message).

    "Model not loaded" is the answer when there is nothing to free, which is the
    normal state at the start of a phase. Reporting it as a failure teaches the
    reader to skim the line, which defeats a memory guard.
    """
    if 200 <= status < 300:
        return True, "unloaded"
    if status in (400, 404) and "not loaded" in body.lower():
        return True, "already free"
    if status == 401:
        return False, "401 — the admin session was not accepted"
    return False, f"HTTP {status} {body.strip()[:120]}"


# ---- I/O (thin wrappers; the logic above is what carries the tests) ---------


def free_gb() -> float:
    return parse_free_gb(subprocess.run(["vm_stat"], capture_output=True, text=True).stdout)


def resident_gb() -> float:
    out = subprocess.run(["ps", "-Ao", "rss,comm"], capture_output=True, text=True).stdout
    return parse_resident_gb(out)


def _request(url: str, cookie: str | None, method: str = "GET", body: dict | None = None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if cookie:
        req.add_header("Cookie", cookie)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode(), r.headers.get("Set-Cookie", "")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), ""
    except OSError as e:
        return 0, str(e), ""


def login(base: str, keys: list[str]) -> str | None:
    """Log in and return the session cookie.

    Admin auth is a login, not a bearer token, and which of oMLX's two
    credentials it accepts is undocumented — so try each and report.
    """
    for key in [k for k in keys if k]:
        status, _, set_cookie = _request(
            f"{base}/admin/api/login", None, "POST", {"api_key": key, "remember": True}
        )
        cookie = set_cookie.split(";")[0] if set_cookie else None
        if 200 <= status < 300 and cookie:
            check, _, _ = _request(f"{base}/admin/api/models", cookie)
            if check == 200:
                return cookie
    return None


def model_size_gb(base: str, cookie: str | None, model_id: str) -> float | None:
    status, body, _ = _request(f"{base}/admin/api/models", cookie)
    return parse_model_size_gb(body, model_id) if status == 200 else None


def unload(base: str, cookie: str | None, model_id: str) -> tuple[bool, str]:
    status, body, _ = _request(f"{base}/admin/api/models/{model_id}/unload", cookie, "POST")
    return classify_unload(status, body)
