#!/usr/bin/env python3
"""Append one line per permission event to <session tmp dir>/gate.jsonl.

Claude Code runs this as an async hook on PreToolUse, PermissionRequest,
PermissionDenied, PostToolUse and PostToolUseFailure, with the event JSON on
stdin. It only records: the exit code is always 0 and nothing is printed, so it
can never block, rewrite or delay a tool call.

The herdr Session Deck reads the file and merges the lines per tool call (see
bin/data.py, `verdicts`). A line carries no tool input or output — the transcript
already has those — only enough to tell who decided:

  {"ts": 1792389010.03, "ev": "pre", "sid": "<session>", "id": "toolu_…",
   "tool": "Bash", "agent": null, "mode": "default", "key": "<input hash>"}

PermissionRequest has no tool_use_id, so every line also carries `key`, a hash of
the tool input, which is what pairs an "ask" with the "pre" of the same call.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import time
from pathlib import Path

EVENTS = {
    "PreToolUse": "pre",
    "PermissionRequest": "ask",
    "PermissionDenied": "denied",
    "PostToolUse": "ok",
    "PostToolUseFailure": "fail",
}


def input_key(tool_input: object) -> str:
    canonical = json.dumps(tool_input, sort_keys=True, ensure_ascii=False, separators=(",", ":"))
    return hashlib.sha1(canonical.encode("utf-8")).hexdigest()[:16]


def session_dir(event: dict) -> Path | None:
    """The session's temp directory, /tmp/claude-<uid>/<project slug>/<session id>.

    `scratchpad_dir` names it exactly when present (its parent is the session
    directory); otherwise it is rebuilt from the transcript's project slug, which
    Claude Code shares between ~/.claude/projects and its temp root.
    """
    session = event.get("session_id")
    if not isinstance(session, str) or not session:
        return None
    scratchpad = event.get("scratchpad_dir")
    if isinstance(scratchpad, str) and scratchpad:
        parent = Path(scratchpad).parent
        if parent.name == session:
            return parent
    transcript = event.get("transcript_path")
    if not isinstance(transcript, str) or not transcript:
        return None
    root = Path(os.environ.get("CLAUDE_CODE_TMPDIR") or "/tmp") / f"claude-{os.getuid()}"
    return root / Path(transcript).parent.name / session


def record(event: dict) -> dict | None:
    kind = EVENTS.get(event.get("hook_event_name") or "")
    if not kind:
        return None
    line = {
        "ts": round(time.time(), 3),
        "ev": kind,
        "sid": event.get("session_id"),
        "id": event.get("tool_use_id"),
        "tool": event.get("tool_name"),
        "agent": event.get("agent_id"),
        "mode": event.get("permission_mode"),
        "key": input_key(event.get("tool_input")),
    }
    if kind == "denied":
        line["reason"] = str(event.get("reason") or "")[:500]
    elif kind == "fail":
        line["error"] = str(event.get("error") or "")[:500]
        if event.get("is_interrupt"):
            line["intr"] = True
    if isinstance(event.get("duration_ms"), (int, float)):
        line["dur"] = event["duration_ms"]
    return line


def main() -> int:
    try:
        event = json.load(sys.stdin)
    except ValueError:
        return 0
    if not isinstance(event, dict):
        return 0
    line = record(event)
    target = session_dir(event)
    if line is None or target is None:
        return 0
    if os.environ.get("SESSION_DECK_HOOK_DEBUG"):
        line["raw_keys"] = sorted(event)
        line["scratchpad_dir"] = event.get("scratchpad_dir")
    try:
        target.mkdir(parents=True, exist_ok=True)
        # One write per line on an O_APPEND descriptor: concurrent hooks of the
        # same session interleave whole lines, never halves.
        fd = os.open(target / "gate.jsonl", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            os.write(fd, (json.dumps(line, ensure_ascii=False) + "\n").encode("utf-8"))
        finally:
            os.close(fd)
    except OSError:
        pass
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:  # noqa: BLE001 - a logging hook must never fail the call it watches
        sys.exit(0)
