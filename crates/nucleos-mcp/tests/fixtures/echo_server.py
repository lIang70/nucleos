#!/usr/bin/env python3
"""A minimal MCP server over stdio: enough to prove the client speaks the protocol.

One tool, `echo`, and it reports its own failure for an empty argument. Everything else
(initialize, tools/list) is the smallest compliant answer.

Three switches exist so the tests can make a source behave like one in production:

- `ECHO_DIE_ON` names a text value that kills the process instead of answering, which is what a
  crash looks like from the client's side: the pipe closes mid-call.
- `ECHO_EXTRA_TOOL` makes the server advertise a second tool, so a reconnect can be made to
  disagree with what the session was promised.
- `ECHO_SPAWN_LOG` appends one line per start, so a test can count how often it was spawned.
"""
import json
import os
import sys

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the text back.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": False,
        },
    }
]


EXTRA_TOOL = {
    "name": "shout",
    "description": "Shout the text back.",
    "inputSchema": {
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
        "additionalProperties": False,
    },
}


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def main():
    log = os.environ.get("ECHO_SPAWN_LOG")
    if log:
        with open(log, "a", encoding="utf-8") as handle:
            handle.write("spawn\n")

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue

        method = message.get("method")
        mid = message.get("id")

        # A notification: no id, no answer.
        if mid is None:
            continue

        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo-fixture", "version": "0.1.0"},
                },
            })
        elif method == "tools/list":
            tools = TOOLS + ([EXTRA_TOOL] if os.environ.get("ECHO_EXTRA_TOOL") else [])
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": tools}})
        elif method == "tools/call":
            params = message.get("params") or {}
            arguments = params.get("arguments") or {}
            text = arguments.get("text")
            if text and text == os.environ.get("ECHO_DIE_ON"):
                # Leave without answering: the client sees a closed pipe, which is the failure a
                # crashed server produces.
                sys.exit(9)
            if not text:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "content": [{"type": "text", "text": "no text to echo"}],
                        "isError": True,
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {"content": [{"type": "text", "text": "echo: " + text}]},
                })
        else:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            })


if __name__ == "__main__":
    main()
