"""Subprocess coverage bootstrap (activated by the pytest-rs-cov .pth).

A Python port of the Rust LineCollector for processes pytest-rs cannot
instrument natively: sys.monitoring with a global PY_START gate, local
LINE (and BRANCH) events on tracked code only, DISABLE after first hit.
Hits and branch arcs are dumped as JSON at exit; the parent pytest-rs
session merges every dump before reporting.

Activation contract (all set by the running pytest-rs session):
    PYTEST_RS_COV_OUT      directory for dump files
    PYTEST_RS_COV_ROOT     rootdir prefix for tracked files
    PYTEST_RS_COV_SOURCES  os.pathsep-joined source filters (optional)
    PYTEST_RS_COV_BRANCH   "1" to record branch arcs
    PYTEST_RS_COV_PATHS    JSON [paths] groups; an alias prefix tracks a file
                           (e.g. a script rsync'd to a worker dir `*/dir1`)
    PYTEST_RS_COV_SIGTERM  "1" to also dump on SIGTERM ([run] sigterm = true)
"""

import os
import re as _re

# coverage.py's _glob_to_regex tokenizer (files.py G2RX_TOKENS). ``None`` means
# the matched text is disallowed; we fall through to a narrower rule instead of
# raising, so ``***`` is consumed one ``*`` at a time.
_G2RX_TOKENS = [
    (_re.compile(rx), sub)
    for rx, sub in [
        (r"\*\*\*+", None),
        (r"[^/]+\*\*+", None),
        (r"\*\*+[^/]+", None),
        (r"\*\*/\*\*", None),
        (r"^\*+/", r"(.*[/\\\\])?"),
        (r"/\*+$", r"[/\\\\].*"),
        (r"\*\*/", r"(.*[/\\\\])?"),
        (r"/", r"[/\\\\]"),
        (r"\*", r"[^/\\\\]*"),
        (r"\?", r"[^/\\\\]"),
        (r"\[.*?\]", r"\g<0>"),
        (r"[a-zA-Z0-9_-]+", r"\g<0>"),
        (r"[\[\]]", None),
        (r".", r"\\\g<0>"),
    ]
]


def _glob_to_regex(pattern):
    """coverage.py _glob_to_regex: a single glob -> unanchored regex string."""
    pattern = pattern.replace("\\", "/")
    if "/" not in pattern:
        pattern = "**/" + pattern
    out = []
    pos = 0
    while pos < len(pattern):
        for rx, sub in _G2RX_TOKENS:
            m = rx.match(pattern, pos)
            if m:
                if sub is not None:
                    out.append(m.expand(sub))
                    pos = m.end()
                    break
                # disallowed token; a narrower rule below handles it.
        else:
            out.append(_re.escape(pattern[pos]))
            pos += 1
    return "".join(out)


def _resolve_alias(pattern, base):
    """coverage.py's `abs_file`: a pattern with no leading wildcard is
    resolved against `base` (the process cwd, upstream) unless already
    absolute — otherwise it can never match an absolute traced filename."""
    if pattern.startswith("*") or os.path.isabs(pattern):
        return pattern
    return os.path.join(base, pattern)


def _compile_path_aliases(raw, base):
    """Compile [paths] alias patterns (JSON groups, canonical first) into
    (regex, canonical) rules: `regex` is a prefix match for the tracked-file
    check, `canonical` is the group's first entry (with a trailing
    separator) used to remap a matched path to its canonical name."""
    if not raw:
        return []
    import json

    try:
        groups = json.loads(raw)
    except (ValueError, TypeError):
        return []
    rules = []
    for group in groups:
        if not isinstance(group, list) or len(group) < 2:
            continue
        canonical = _resolve_alias(group[0], base).rstrip("/\\") + os.sep
        for alias in group[1:]:
            if not isinstance(alias, str):
                continue
            pat = alias.rstrip("/\\")
            if not pat or pat.endswith("*"):
                continue
            pat = _resolve_alias(pat, base)
            rules.append((_re.compile(_glob_to_regex(pat + "/"), _re.IGNORECASE), canonical))
    return rules


def start():
    out_dir = os.environ.get("PYTEST_RS_COV_OUT")
    if not out_dir:
        return
    import atexit
    import json
    import sys

    root = os.environ.get("PYTEST_RS_COV_ROOT", "")
    sources = [s for s in os.environ.get("PYTEST_RS_COV_SOURCES", "").split(os.pathsep) if s]
    branch = os.environ.get("PYTEST_RS_COV_BRANCH") == "1"
    sigterm = os.environ.get("PYTEST_RS_COV_SIGTERM") == "1"
    alias_rules = _compile_path_aliases(os.environ.get("PYTEST_RS_COV_PATHS"), root)

    mon = sys.monitoring
    tool = mon.COVERAGE_ID
    try:
        mon.use_tool_id(tool, "pytest-rs-cov-child")
    except ValueError:
        return  # someone else measures this process

    hits = {}
    arcs = {}
    tracked = {}  # id(code) -> (filename, co_lines table, jump targets)
    local_events = mon.events.LINE
    left = getattr(mon.events, "BRANCH_LEFT", None)
    right = getattr(mon.events, "BRANCH_RIGHT", None)
    combined = None
    if branch:
        if left is not None and right is not None:
            local_events |= left | right
        else:
            combined = mon.events.BRANCH
            local_events |= combined
    seen_dests = {}

    def is_tracked(filename):
        if filename.startswith("<") or "site-packages" in filename or "/lib/python" in filename:
            return False
        if sources:
            if any(filename == s.rstrip(os.sep) or filename.startswith(s) for s in sources):
                return True
        elif filename.startswith(root):
            return True
        # coverage [paths] aliases: a file under an alias (e.g. a script
        # rsync'd to a worker dir matched by `*/dir1`) is tracked too.
        return any(rx.match(filename) for rx, _ in alias_rules)

    def _map_alias(filename):
        """[paths] remap: the first alias whose prefix matches `filename`
        rewrites the matched prefix to its canonical result, if that
        canonical path actually exists (mirrors coverage.py's PathAliases.map
        and the Rust LineCollector's `canonical_name`)."""
        for rx, canonical in alias_rules:
            m = rx.match(filename)
            if m:
                new = canonical + filename[m.end() :]
                if os.path.exists(new):
                    return new
        return filename

    def py_start(code, _offset):
        key = id(code)
        if key not in tracked:
            filename = code.co_filename
            if code.co_name != "__annotate__" and is_tracked(filename):
                table = sorted(
                    (start_, end, line if line is not None else -1)
                    for start_, end, line in code.co_lines()
                )
                jumps = None
                if combined is not None:
                    import dis

                    conditional = {
                        "POP_JUMP_IF_FALSE",
                        "POP_JUMP_IF_TRUE",
                        "POP_JUMP_IF_NONE",
                        "POP_JUMP_IF_NOT_NONE",
                        "FOR_ITER",
                    }
                    jumps = {
                        i.offset: i.argval
                        for i in dis.get_instructions(code)
                        if i.opname in conditional and isinstance(i.argval, int)
                    }
                tracked[key] = (_map_alias(filename), table, jumps, code)
                mon.set_local_events(tool, code, local_events)
        return mon.DISABLE

    def line_at(table, offset):
        for start_, end, line in table:
            if start_ <= offset < end:
                return line
        return -1

    def line(code, lineno):
        entry = tracked.get(id(code))
        if entry is not None:
            hits.setdefault(entry[0], set()).add(lineno)
        return mon.DISABLE

    def record(code, src_offset, dst_offset, direction):
        entry = tracked.get(id(code))
        if entry is None:
            return
        filename, table, jumps, _ = entry
        if direction == 0 and jumps is not None:
            target = jumps.get(src_offset)
            if target is None:
                direction = 0
            elif (target >= src_offset and dst_offset >= target) or (
                target < src_offset and dst_offset == target
            ):
                direction = 2
            else:
                direction = 1
        src = line_at(table, src_offset)
        if src <= 0:
            return
        arcs.setdefault(filename, set()).add((src, line_at(table, dst_offset), direction))

    def branch_left(code, src_offset, dst_offset):
        record(code, src_offset, dst_offset, 1)
        return mon.DISABLE

    def branch_right(code, src_offset, dst_offset):
        record(code, src_offset, dst_offset, 2)
        return mon.DISABLE

    def branch_compat(code, src_offset, dst_offset):
        record(code, src_offset, dst_offset, 0)
        dests = seen_dests.setdefault((id(code), src_offset), set())
        dests.add(dst_offset)
        if len(dests) >= 2:
            return mon.DISABLE
        return None

    mon.register_callback(tool, mon.events.PY_START, py_start)
    mon.register_callback(tool, mon.events.LINE, line)
    if branch:
        if combined is None:
            mon.register_callback(tool, left, branch_left)
            mon.register_callback(tool, right, branch_right)
        else:
            mon.register_callback(tool, combined, branch_compat)
    mon.set_events(tool, mon.events.PY_START)

    def dump():
        try:
            mon.set_events(tool, 0)
        except Exception:
            pass
        if not hits and not arcs:
            return
        payload = {
            "hits": {f: sorted(lines) for f, lines in hits.items()},
            "arcs": {f: sorted(list(arc) for arc in file_arcs) for f, file_arcs in arcs.items()},
        }
        path = os.path.join(out_dir, f"child-{os.getpid()}-{id(hits):x}.json")
        try:
            with open(path, "w") as fh:
                json.dump(payload, fh)
        except OSError:
            pass

    atexit.register(dump)

    if sigterm:
        import signal

        old_sigterm = signal.getsignal(signal.SIGTERM)

        def on_sigterm(signum, frame):
            # atexit never runs on an unhandled SIGTERM — dump now, then
            # restore the original disposition and re-raise so the process
            # still dies the same way it would have (coverage.py's
            # Coverage._on_sigterm).
            dump()
            signal.signal(signal.SIGTERM, old_sigterm)
            os.kill(os.getpid(), signal.SIGTERM)

        try:
            signal.signal(signal.SIGTERM, on_sigterm)
        except (ValueError, OSError):
            pass  # not the main thread, or platform doesn't support it


if __name__ == "pytest_rs_cov_child":  # via the .pth runpy loader
    start()
