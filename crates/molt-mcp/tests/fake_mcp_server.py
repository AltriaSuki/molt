"""A tiny MCP server on stdio, for Molt's tests. No network.

Usage: fake_mcp_server.py DIR

Tools:
- echo {text}: answers "echo: TEXT". Its description is the contents of
  DIR/description when that file exists, else "Echo the text back.".
- fail {}: answers with isError.
- crash {}: writes to stderr and exits without answering.
- redescribe {description}: gives echo this description from now on, and
  says the tool list changed.
- probe {path}: says what the server can reach: the network, whether
  a path exists, some variables.
"""

import json
import os
import sys

DIR = sys.argv[1] if len(sys.argv) > 1 else "."
REDESCRIBED = None


def description():
    if REDESCRIBED is not None:
        return REDESCRIBED
    try:
        with open(os.path.join(DIR, "description")) as f:
            return f.read().strip()
    except OSError:
        return "Echo the text back."


def tools():
    closed = {"type": "object", "properties": {}, "additionalProperties": False}
    return [
        {
            "name": "echo",
            "description": description(),
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "What to echo."}},
                "required": ["text"],
                "additionalProperties": False,
            },
            "annotations": {"readOnlyHint": True},
        },
        {"name": "fail", "description": "Always fails.", "inputSchema": closed},
        {"name": "crash", "description": "Exits without answering.", "inputSchema": closed},
        {"name": "redescribe", "description": "Changes echo's description.", "inputSchema": {"type": "object"}},
        {"name": "probe", "description": "Says what it can reach.", "inputSchema": {"type": "object"}},
    ]


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def answer(request):
    method, params = request.get("method"), request.get("params") or {}
    if method == "initialize":
        return {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "fake", "version": "1"},
        }
    if method == "tools/list":
        return {"tools": tools()}
    if method == "tools/call":
        name, args = params.get("name"), params.get("arguments") or {}
        if name == "echo":
            return {"content": [{"type": "text", "text": "echo: " + args.get("text", "")}]}
        if name == "fail":
            return {"content": [{"type": "text", "text": "it failed on purpose"}], "isError": True}
        if name == "crash":
            sys.stderr.write("fake server crashing on purpose\n")
            sys.stderr.flush()
            os._exit(3)
        if name == "redescribe":
            global REDESCRIBED
            REDESCRIBED = args.get("description", "")
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            return {"content": [{"type": "text", "text": "redescribed"}]}
        if name == "probe":
            import socket

            try:
                socket.create_connection(("1.1.1.1", 53), timeout=1).close()
                network = True
            except OSError:
                network = False
            seen = {
                "network": network,
                "home": os.environ.get("HOME"),
                "exists": os.path.exists(args.get("path", "/nonexistent")),
                "given": os.environ.get("GIVEN_TO_SERVER"),
                "not_given": os.environ.get("NOT_GIVEN_TO_SERVER"),
            }
            return {"content": [{"type": "text", "text": json.dumps(seen, sort_keys=True)}]}
        raise KeyError(name)
    raise KeyError(method)


for line in sys.stdin:
    if not line.strip():
        continue
    request = json.loads(line)
    if "id" not in request:
        continue
    try:
        send({"jsonrpc": "2.0", "id": request["id"], "result": answer(request)})
    except KeyError as e:
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "unknown " + str(e)}})
