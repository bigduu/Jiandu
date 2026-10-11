#!/usr/bin/env python3
"""Smoke a built/extracted CLI using only a fresh temporary data directory."""

import argparse
import json
import queue
import subprocess
import tempfile
import threading
import time
from pathlib import Path


def smoke(binary, version):
    with tempfile.TemporaryDirectory(prefix="jiandu-plugin-smoke-") as temporary:
        root = Path(temporary) / "memory"
        help_result = subprocess.run([str(binary), "--help"], capture_output=True, text=True, timeout=15, check=True)
        if "--data-dir" not in help_result.stdout or root.exists():
            raise ValueError("CLI help or isolated data root check failed")
        process = subprocess.Popen([str(binary), "--data-dir", str(root)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        responses = queue.Queue()

        def read():
            for line in process.stdout:
                try:
                    responses.put(json.loads(line))
                except ValueError as error:
                    responses.put(error)

        reader = threading.Thread(target=read, daemon=True)
        reader.start()

        def send(value):
            process.stdin.write(json.dumps(value) + "\n")
            process.stdin.flush()

        def receive(request_id):
            deadline = time.monotonic() + 15
            while True:
                response = responses.get(timeout=max(0, deadline - time.monotonic()))
                if isinstance(response, Exception):
                    raise response
                if response.get("id") == request_id:
                    if "error" in response:
                        raise ValueError("MCP protocol error: " + json.dumps(response["error"]))
                    return response["result"]

        try:
            send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "jiandu-package-smoke", "version": "1"}}})
            initialized = receive(1)
            if initialized["serverInfo"]["name"] != "jiandu" or initialized["serverInfo"]["version"] != version:
                raise ValueError("built binary identity differs from the candidate version")
            send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            if [tool["name"] for tool in receive(2)["tools"]] != ["memory"]:
                raise ValueError("expected the single Jiandu memory tool")
            if root.exists():
                raise ValueError("startup or tool discovery unexpectedly initialized memory")
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            reader.join(timeout=5)
            process.stdout.close()
            process.stderr.close()
        if process.returncode != 0:
            raise ValueError("Jiandu did not exit cleanly after MCP EOF")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--version", required=True)
    arguments = parser.parse_args()
    smoke(arguments.binary.resolve(strict=True), arguments.version)
    print("verified CLI help, exact MCP version, one memory tool, and no store initialization")
