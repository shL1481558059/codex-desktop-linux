#!/usr/bin/env python3
"""Repair the one known malformed Computer Use image-format argument shape."""

from __future__ import annotations

import json
import sys
from typing import Optional


ALLOWED_TOOLS = {
    "mcp__computer_use__get_app_state",
    "mcp__computer_use__screenshot",
}
ALLOWED_FORMATS = {"jpeg", "png"}


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


def main() -> int:
    try:
        event = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        print("{}")
        return 0

    if not isinstance(event, dict) or event.get("tool_name") not in ALLOWED_TOOLS:
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
