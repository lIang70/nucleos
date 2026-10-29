#!/usr/bin/env python3
"""A minimal MCP server over stdio: enough to prove the client speaks the protocol.

One tool, `echo`, and it reports its own failure for an empty argument. Everything else
(initialize, tools/list) is the smallest compliant answer.
"""
import json
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


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def main():
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
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
        elif method == "tools/call":
            params = message.get("params") or {}
            arguments = params.get("arguments") or {}
            text = arguments.get("text")
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
