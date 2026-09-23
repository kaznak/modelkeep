#!/usr/bin/env python3
"""Fail-closed audit: every `event = "..."` emitted by src/ (outside #[cfg(test)]
modules) must have an entry in the structured operational event reference table.

This does not parse Rust with a real compiler front end. It tokenizes just enough
(comments, string/char literals, brace/paren nesting) to:

  1. find `#[cfg(test)] mod <name> { ... }` blocks and exclude everything inside
     them from consideration, and
  2. find each `tracing::{trace,debug,info,warn,error}!(...)` call and, when the
     call has an `event` field, read its value.

Anywhere the `event` field's value cannot be confirmed to be a plain string
literal (for example, a variable, a `format!(...)` call, or any other
expression), the script fails closed: it reports the call site as
undeterminable and exits non-zero rather than silently skipping it. This is
required by docs/issues/0075: a check that only reads the table cannot detect
the one-directional gap between emitted events and documented ones, so this
check reads src/ instead.

Usage: structured_event_reference_check.py <src-dir> <reference-doc>
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

LEVELS = ("trace", "debug", "info", "warn", "error")

MACRO_START_RE = re.compile(r"tracing::(" + "|".join(LEVELS) + r")!\s*\(")
CFG_TEST_MOD_RE = re.compile(
    r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*mod\s+\w+\s*\{"
)
EVENT_KEY_RE = re.compile(r"\bevent\b\s*=\s*")
TABLE_ROW_RE = re.compile(r"^\|\s*`([a-zA-Z0-9_]+)`\s*\|")


@dataclass
class Segment:
    kind: str  # 'CODE', 'STRING', 'CHAR', 'COMMENT'
    text: str
    start_line: int


@dataclass
class EmittedEvent:
    name: str
    file: str
    line: int
    level: str


@dataclass
class Undeterminable:
    file: str
    line: int
    level: str
    detail: str


def try_char_literal(s: str, i: int) -> int | None:
    """If s[i] == "'" begins a char literal, return the index just past its
    closing quote. Otherwise return None (treat the quote as a lifetime mark
    or other stray punctuation, not a string-like token)."""
    n = len(s)
    j = i + 1
    if j >= n:
        return None
    if s[j] == "\\":
        j += 1
        if j >= n:
            return None
        esc = s[j]
        j += 1
        if esc == "u":
            if j < n and s[j] == "{":
                close = s.find("}", j)
                if close == -1:
                    return None
                j = close + 1
            else:
                return None
        elif esc == "x":
            j += 2
        if j < n and s[j] == "'":
            return j + 1
        return None
    else:
        j += 1
        if j < n and s[j] == "'":
            return j + 1
        return None


def try_raw_string(text: str, i: int) -> int | None:
    """If text[i:] begins a raw string body 'r"...' or 'r#..#"...' (the 'r' at
    i has already been confirmed to be at a word boundary), return the index
    just past its closing delimiter. Otherwise None."""
    n = len(text)
    j = i + 1
    hashes = 0
    while j < n and text[j] == "#":
        hashes += 1
        j += 1
    if j >= n or text[j] != '"':
        return None
    j += 1
    terminator = '"' + ("#" * hashes)
    end = text.find(terminator, j)
    if end == -1:
        return None
    return end + len(terminator)


def word_boundary(code_buf: list[str]) -> bool:
    if not code_buf:
        return True
    prev = code_buf[-1]
    return not (prev.isalnum() or prev == "_")


def tokenize(text: str) -> list[Segment]:
    n = len(text)
    i = 0
    line = 1
    segments: list[Segment] = []
    code_buf: list[str] = []
    code_start_line = 1

    def flush_code() -> None:
        if code_buf:
            segments.append(Segment("CODE", "".join(code_buf), code_start_line))
            code_buf.clear()

    def scan_quoted_string(start: int) -> int:
        """text[start] == '"'; return index just past the matching closing
        quote, handling backslash escapes. Also advances the outer `line`
        counter for embedded newlines."""
        nonlocal line
        j = start + 1
        while j < n:
            if text[j] == "\\" and j + 1 < n:
                j += 2
                continue
            if text[j] == "\n":
                line += 1
            if text[j] == '"':
                j += 1
                break
            j += 1
        return j

    while i < n:
        c = text[i]
        if text[i : i + 2] == "//":
            flush_code()
            start = i
            start_line = line
            while i < n and text[i] != "\n":
                i += 1
            segments.append(Segment("COMMENT", text[start:i], start_line))
            continue
        if text[i : i + 2] == "/*":
            flush_code()
            start = i
            start_line = line
            depth = 1
            i += 2
            while i < n and depth > 0:
                if text[i : i + 2] == "/*":
                    depth += 1
                    i += 2
                elif text[i : i + 2] == "*/":
                    depth -= 1
                    i += 2
                else:
                    if text[i] == "\n":
                        line += 1
                    i += 1
            segments.append(Segment("COMMENT", text[start:i], start_line))
            continue
        if c == '"':
            flush_code()
            start = i
            start_line = line
            i = scan_quoted_string(i)
            segments.append(Segment("STRING", text[start:i], start_line))
            continue
        if c == "'":
            end = try_char_literal(text, i)
            if end is not None:
                flush_code()
                segments.append(Segment("CHAR", text[i:end], line))
                i = end
                continue
            # Lifetime mark or stray quote: fall through as an ordinary code
            # character.
        if c == "r" and word_boundary(code_buf):
            end = try_raw_string(text, i)
            if end is not None:
                flush_code()
                start_line = line
                consumed = text[i:end]
                segments.append(Segment("STRING", consumed, start_line))
                line += consumed.count("\n")
                i = end
                continue
        if c == "b" and word_boundary(code_buf) and i + 1 < n:
            if text[i + 1] == '"':
                flush_code()
                start = i
                start_line = line
                i = scan_quoted_string(i + 1)
                segments.append(Segment("STRING", text[start:i], start_line))
                continue
            if text[i + 1] == "'":
                end = try_char_literal(text, i + 1)
                if end is not None:
                    flush_code()
                    segments.append(Segment("CHAR", text[i:end], line))
                    i = end
                    continue
            if text[i + 1] == "r":
                end = try_raw_string(text, i + 1)
                if end is not None:
                    flush_code()
                    start_line = line
                    consumed = text[i:end]
                    segments.append(Segment("STRING", consumed, start_line))
                    line += consumed.count("\n")
                    i = end
                    continue
        if not code_buf:
            code_start_line = line
        code_buf.append(c)
        if c == "\n":
            line += 1
        i += 1
    flush_code()
    return segments


def build_code_stream(segments: list[Segment]) -> tuple[str, list[int]]:
    chars: list[str] = []
    lines: list[int] = []
    for seg in segments:
        if seg.kind != "CODE":
            continue
        cur_line = seg.start_line
        for ch in seg.text:
            chars.append(ch)
            lines.append(cur_line)
            if ch == "\n":
                cur_line += 1
    return "".join(chars), lines


def find_test_exclusion_ranges(segments: list[Segment]) -> list[tuple[int, int]]:
    code, lines = build_code_stream(segments)
    ranges: list[tuple[int, int]] = []
    for m in CFG_TEST_MOD_RE.finditer(code):
        start_line = lines[m.start()]
        open_brace = m.end() - 1
        assert code[open_brace] == "{"
        depth = 1
        k = open_brace + 1
        while k < len(code) and depth > 0:
            if code[k] == "{":
                depth += 1
            elif code[k] == "}":
                depth -= 1
            k += 1
        if depth != 0:
            # Brace nesting never closed: our tokenizer/brace-matcher could not
            # make sense of this file. Fail closed rather than guess at the
            # exclusion boundary.
            raise RuntimeError(
                f"could not find the closing brace of a #[cfg(test)] mod block "
                f"starting at line {start_line}; refusing to guess"
            )
        end_line = lines[k - 1]
        ranges.append((start_line, end_line))
    return ranges


def line_excluded(line: int, ranges: list[tuple[int, int]]) -> bool:
    return any(start <= line <= end for start, end in ranges)


def find_macro_calls(segments: list[Segment]) -> list[tuple[int, int, str, int]]:
    """Return (segment_index, offset_after_open_paren, level, call_line) for
    every tracing::<level>!( occurrence in CODE segments."""
    calls = []
    for idx, seg in enumerate(segments):
        if seg.kind != "CODE":
            continue
        for m in MACRO_START_RE.finditer(seg.text):
            call_line = seg.start_line + seg.text[: m.start()].count("\n")
            calls.append((idx, m.end(), m.group(1), call_line))
    return calls


def extract_call_chunks(
    segments: list[Segment], seg_idx: int, offset: int
) -> tuple[list[Segment], int, int]:
    """Walk forward from (seg_idx, offset) — the position right after a macro
    call's opening paren — tracking paren depth across CODE segment text only,
    to find the matching closing paren. Returns the ordered list of chunks
    making up the call's argument list (with the first/last CODE segment
    sliced to the call's exact bounds), plus the segment index and offset
    where the call ends (for resuming an outer scan, unused here but kept for
    clarity)."""
    depth = 1
    chunks: list[Segment] = []
    i = seg_idx
    start_offset = offset
    n = len(segments)
    while i < n:
        seg = segments[i]
        if seg.kind != "CODE":
            chunks.append(seg)
            i += 1
            continue
        text = seg.text
        begin = start_offset if i == seg_idx else 0
        k = begin
        while k < len(text):
            ch = text[k]
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0:
                    k += 1
                    break
            k += 1
        chunks.append(Segment("CODE", text[begin:k], seg.start_line))
        if depth == 0:
            return chunks, i, k
        i += 1
    raise RuntimeError(
        "reached end of file while looking for the closing paren of a "
        "tracing::*!( call; refusing to guess"
    )


def find_event_value(
    chunks: list[Segment], file: str, call_line: int, level: str
) -> tuple[str | None, Undeterminable | None]:
    for idx, chunk in enumerate(chunks):
        if chunk.kind != "CODE":
            continue
        m = EVENT_KEY_RE.search(chunk.text)
        if not m:
            continue
        remainder = chunk.text[m.end() :]
        if remainder.strip():
            # Something other than whitespace follows `event =` in the same
            # code chunk, so the value is an inline expression, not a bare
            # string literal.
            snippet = remainder.strip().splitlines()[0][:60]
            return None, Undeterminable(
                file, call_line, level, f"event value is not a string literal: {snippet!r}"
            )
        # The value continues past this CODE chunk; it must be the very next
        # chunk, and that chunk must be a plain string literal.
        if idx + 1 >= len(chunks):
            return None, Undeterminable(
                file, call_line, level, "event field has no value before the call ends"
            )
        nxt = chunks[idx + 1]
        if nxt.kind != "STRING":
            return None, Undeterminable(
                file,
                call_line,
                level,
                f"event value is a {nxt.kind.lower()} token, not a string literal",
            )
        literal = nxt.text
        if not (literal.startswith('"') and literal.endswith('"') and len(literal) >= 2):
            return None, Undeterminable(
                file, call_line, level, f"unrecognized string literal form: {literal!r}"
            )
        inner = literal[1:-1]
        if "\\" in inner:
            # Every event name observed in this codebase is a plain
            # snake_case identifier with no escapes. Refuse to guess at
            # unescaping rules for anything fancier.
            return None, Undeterminable(
                file, call_line, level, f"event literal contains an escape: {literal!r}"
            )
        return inner, None
    return None, None  # no `event` field on this call at all; not an error


def collect_emitted_events(
    path: Path, rel_name: str
) -> tuple[list[EmittedEvent], list[Undeterminable]]:
    text = path.read_text(encoding="utf-8")
    segments = tokenize(text)
    exclusions = find_test_exclusion_ranges(segments)
    events: list[EmittedEvent] = []
    problems: list[Undeterminable] = []
    for seg_idx, offset, level, call_line in find_macro_calls(segments):
        excluded = line_excluded(call_line, exclusions)
        chunks, _end_seg, _end_offset = extract_call_chunks(segments, seg_idx, offset)
        name, problem = find_event_value(chunks, rel_name, call_line, level)
        if excluded:
            # Test code is out of scope for the reference entirely: skip both
            # successful matches and undeterminable ones.
            continue
        if problem is not None:
            problems.append(problem)
            continue
        if name is not None:
            events.append(EmittedEvent(name, rel_name, call_line, level.upper()))
    return events, problems


def documented_events(doc_path: Path) -> set[str]:
    names: set[str] = set()
    for line in doc_path.read_text(encoding="utf-8").splitlines():
        m = TABLE_ROW_RE.match(line)
        if m:
            names.add(m.group(1))
    return names


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"usage: {argv[0]} <src-dir> <reference-doc>", file=sys.stderr)
        return 2
    src_dir = Path(argv[1])
    doc_path = Path(argv[2])

    rs_files = sorted(src_dir.rglob("*.rs"))
    if not rs_files:
        print(f"no .rs files found under {src_dir}; refusing to pass vacuously", file=sys.stderr)
        return 2

    all_events: list[EmittedEvent] = []
    all_problems: list[Undeterminable] = []
    for path in rs_files:
        rel = path.relative_to(src_dir.parent) if src_dir.parent in path.parents else path
        events, problems = collect_emitted_events(path, str(rel))
        all_events.extend(events)
        all_problems.extend(problems)

    if all_problems:
        print(
            "structured-event-reference: found tracing calls whose `event` "
            "field could not be statically confirmed as a string literal; "
            "failing closed rather than guessing:",
            file=sys.stderr,
        )
        for p in all_problems:
            print(f"  {p.file}:{p.line}: [{p.level}] {p.detail}", file=sys.stderr)
        return 1

    emitted_names = {e.name for e in all_events}
    documented = documented_events(doc_path)

    undocumented = sorted(emitted_names - documented)
    unemitted = sorted(documented - emitted_names)

    ok = True
    if undocumented:
        ok = False
        print(
            f"structured-event-reference: {len(undocumented)} event(s) emitted "
            f"by src/ have no entry in {doc_path}:",
            file=sys.stderr,
        )
        for name in undocumented:
            sites = sorted(
                {f"{e.file}:{e.line}" for e in all_events if e.name == name}
            )
            print(f"  {name}  (e.g. {sites[0]})", file=sys.stderr)

    if unemitted:
        ok = False
        print(
            f"structured-event-reference: {len(unemitted)} event(s) documented "
            f"in {doc_path} are not emitted anywhere in src/ (outside tests):",
            file=sys.stderr,
        )
        for name in unemitted:
            print(f"  {name}", file=sys.stderr)

    if not ok:
        return 1

    print(
        f"structured-event-reference: {len(emitted_names)} emitted event(s) "
        f"all documented; {len(documented)} documented event(s) all emitted."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
