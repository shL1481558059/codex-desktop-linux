#!/usr/bin/env python3
"""Repair legacy image JSON and block direct shell pointer injection."""

from __future__ import annotations

import json
import os
import re
import shlex
import sys
from typing import Optional


ALLOWED_TOOLS = {
    "mcp__computer_use__get_app_state",
    "mcp__computer_use__screenshot",
}
ALLOWED_FORMATS = {"jpeg", "png"}
SHELL_TOOLS = {"exec_command", "Bash", "shell"}
SHELL_WRAPPERS = {"command", "env", "nohup", "sudo"}


def skip_whitespace(text: str, index: int) -> int:
    while index < len(text) and text[index].isspace():
        index += 1
    return index


def scan_string(text: str, index: int) -> Optional[int]:
    if index >= len(text) or text[index] != '"':
        return None
    index += 1
    escaped = False
    while index < len(text):
        character = text[index]
        if escaped:
            escaped = False
        elif character == "\\":
            escaped = True
        elif character == '"':
            return index + 1
        index += 1
    return None


def scan_compound(text: str, index: int) -> Optional[int]:
    if index >= len(text) or text[index] not in "[{":
        return None
    stack = [text[index]]
    index += 1
    matching = {"}": "{", "]": "["}
    while index < len(text) and stack:
        character = text[index]
        if character == '"':
            index = scan_string(text, index)
            if index is None:
                return None
            continue
        if character in "[{":
            stack.append(character)
        elif character in "]}":
            if matching[character] != stack[-1]:
                return None
            stack.pop()
        index += 1
    return index if not stack else None


def scan_primitive(text: str, index: int) -> Optional[int]:
    start = index
    while index < len(text) and not text[index].isspace() and text[index] not in ",}]":
        index += 1
    return index if index > start else None


def scan_value(text: str, index: int) -> Optional[int]:
    if index >= len(text):
        return None
    if text[index] == '"':
        return scan_string(text, index)
    if text[index] in "[{":
        return scan_compound(text, index)
    return scan_primitive(text, index)


def repair_tool_input(raw_input: object) -> Optional[dict]:
    if not isinstance(raw_input, str):
        return None

    text = raw_input
    index = skip_whitespace(text, 0)
    if index >= len(text) or text[index] != "{":
        return None
    index += 1
    format_occurrences = 0
    replacement: Optional[tuple[int, int, str]] = None

    while True:
        index = skip_whitespace(text, index)
        if index >= len(text):
            return None
        if text[index] == "}":
            index += 1
            break

        key_start = index
        key_end = scan_string(text, index)
        if key_end is None:
            return None
        try:
            key = json.loads(text[key_start:key_end])
        except json.JSONDecodeError:
            return None
        index = skip_whitespace(text, key_end)
        if index >= len(text) or text[index] != ":":
            return None
        index = skip_whitespace(text, index + 1)
        value_start = index

        if key == "format":
            format_occurrences += 1
            token_end = scan_primitive(text, value_start)
            if token_end is not None:
                token = text[value_start:token_end]
                if token in ALLOWED_FORMATS:
                    replacement = (value_start, token_end, token)

        value_end = scan_value(text, value_start)
        if value_end is None:
            return None
        index = skip_whitespace(text, value_end)
        if index < len(text) and text[index] == ",":
            index += 1
            continue
        if index < len(text) and text[index] == "}":
            index += 1
            break
        return None

    if skip_whitespace(text, index) != len(text):
        return None
    if format_occurrences != 1 or replacement is None:
        return None

    start, end, token = replacement
    repaired_text = text[:start] + json.dumps(token) + text[end:]
    try:
        repaired = json.loads(repaired_text)
    except json.JSONDecodeError:
        return None
    return repaired if isinstance(repaired, dict) else None


def shell_command(raw_input: object) -> Optional[str]:
    if isinstance(raw_input, str):
        return raw_input
    if not isinstance(raw_input, dict):
        return None
    for key in ("cmd", "command"):
        value = raw_input.get(key)
        if isinstance(value, str):
            return value
    return None


def denied_shell_input(tool_name: object, raw_input: object) -> bool:
    if tool_name not in SHELL_TOOLS:
        return False
    command = shell_command(raw_input)
    if command is None:
        return False
    for segment in re.split(r"[;&|()\n]+", command):
        try:
            tokens = shlex.split(segment, comments=True)
        except ValueError:
            continue
        index = 0
        while index < len(tokens):
            token = tokens[index]
            if "=" in token and not token.startswith("/"):
                index += 1
                continue
            executable = os.path.basename(token)
            if executable in SHELL_WRAPPERS:
                index += 1
                while index < len(tokens) and tokens[index].startswith("-"):
                    index += 1
                continue
            if executable in {"xdotool", "ydotool"}:
                return True
            break
    return False


def main() -> int:
    try:
        event = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        print("{}")
        return 0

    if not isinstance(event, dict):
        print("{}")
        return 0

    tool_name = event.get("tool_name")
    if denied_shell_input(tool_name, event.get("tool_input")):
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": (
                            "Direct shell xdotool/ydotool input is disabled. "
                            "Use the Computer Use click, scroll, drag, keyboard, or text tools."
                        ),
                    }
                },
                ensure_ascii=False,
                separators=(",", ":"),
            )
        )
        return 0

    if tool_name not in ALLOWED_TOOLS:
        print("{}")
        return 0

    repaired = repair_tool_input(event.get("tool_input"))
    if repaired is None:
        print("{}")
        return 0

    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "updatedInput": repaired,
                }
            },
            ensure_ascii=False,
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
