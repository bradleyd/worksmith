"""Deterministic offline MCP peer. Never contacts a service or executes input."""
import json
import os
import sys
import time

log, mode = sys.argv[1:3]
def record(value):
    with open(log, "a") as out:
        out.write(json.dumps(value) + "\n")

def send(value):
    print(json.dumps(value), flush=True)

record({"start": os.getpid(), "cwd": os.getcwd(), "home_inherited": "HOME" in os.environ})
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request["method"], request.get("id")
    if method == "initialize":
        if mode == "init_hang":
            time.sleep(60)
        result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {"listChanged": True}}, "serverInfo": {"name": "fixture", "version": "1"}, "instructions": "UNTRUSTED_SERVER_INSTRUCTIONS_DO_NOT_INJECT"}
    elif method == "tools/list":
        schema = {"type": "object", "properties": {"value": {"type": "string"}}}
        if mode == "bad_schema":
            schema = {"type": "object", "$ref": "https://invalid.example/schema"}
        names = ["echo", "mutate", "hang", "large", "error", "image", "change"]
        result = {"tools": [{"name": name, "description": "fixture " + name, "inputSchema": schema} for name in names]}
        if mode == "pagination_loop":
            result = {"tools": [], "nextCursor": "same"}
        if mode == "oversized_frame":
            print("x" * 2000000, flush=True)
            continue
    elif method == "tools/call":
        name = request["params"]["name"]
        record({"call": name, "arguments": request["params"].get("arguments")})
        if name == "mutate":
            os._exit(0)
        if name == "hang":
            time.sleep(60)
        if name == "change":
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        if request["params"].get("arguments", {}).get("value") == "invalid":
            send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32602, "message": "do not echo secret error data"}})
            continue
        content = "fixture result"
        if name == "echo":
            content = json.dumps(request["params"].get("arguments"))
        if name == "large":
            content = "é" * 12000 + "END_OF_RESULT"
        result = {"content": [{"type": "text", "text": content}], "isError": name == "error"}
        if name == "image":
            result = {"content": [{"type": "image", "data": "AA==", "mimeType": "image/png"}]}
    elif ident is None:
        if method == "notifications/cancelled":
            record({"cancelled": True})
        continue
    else:
        send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32601, "message": "unsupported"}})
        continue
    send({"jsonrpc": "2.0", "id": ident, "result": result})
