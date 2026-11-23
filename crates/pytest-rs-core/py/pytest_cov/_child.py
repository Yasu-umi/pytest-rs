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
    PYTEST_RS_COV_OMIT     JSON list of [run] omit patterns (prepped)
    PYTEST_RS_COV_INCLUDE  JSON list of [run] include patterns (prepped; only
                           set without a source)
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


def _multiline_map(filename):
    """Continuation line -> first line of its logical line (coverage.py's
    multiline map, from the tokenizer)."""
    import tokenize

    out = {}
    skip = {tokenize.NL, tokenize.COMMENT, tokenize.INDENT, tokenize.DEDENT, tokenize.ENCODING}
    try:
        with tokenize.open(filename) as fh:
            first = None
            for tok in tokenize.generate_tokens(fh.readline):
                if tok.type == tokenize.NEWLINE:
                    if first is not None:
                        for line in range(first + 1, tok.end[0] + 1):
                            out[line] = first
                    first = None
                elif tok.type not in skip and first is None:
                    first = tok.start[0]
    except (OSError, SyntaxError, tokenize.TokenError):
        return {}
    return out


def _resolve_arcs(code, pairs, multiline):
    """coverage.py's BranchArcResolver: (source, dest) offsets -> line arcs.

    From the destination, follow the instructions (through unconditional
    jumps) to the first one on another line (the arc), a return (an arc
    leaving the code object), or another branch (no arc)."""
    import dis

    opmap = dis.opmap
    cache = opmap.get("CACHE", -1)
    extended_arg = opmap["EXTENDED_ARG"]
    jumps = set(dis.hasjrel) | set(dis.hasjabs)
    always = {
        opmap[n]
        for n in ("JUMP_BACKWARD", "JUMP_BACKWARD_NO_INTERRUPT", "JUMP_FORWARD")
        if n in opmap
    }
    backward = {op for op in jumps if "JUMP_BACKWARD" in dis.opname[op]}
    returns = {opmap[n] for n in ("RETURN_VALUE", "RETURN_GENERATOR", "RETURN_CONST") if n in opmap}
    no_fall = {opmap[n] for n in ("RERAISE",) if n in opmap}
    byte_to_line = {}
    for start_, end, line in code.co_lines():
        if line is not None:
            for offset in range(start_, end, 2):
                byte_to_line[offset] = line
    co_code = code.co_code
    out = set()
    for source, dest in pairs:
        from_line = byte_to_line.get(source)
        if from_line is None:
            continue
        from_line = multiline.get(from_line, from_line)
        offset, ext_arg, seen = dest, 0, set()
        while 0 <= offset < len(co_code) and offset not in seen:
            seen.add(offset)
            op = co_code[offset]
            if op == cache:
                offset += 2
                continue
            if op == extended_arg:
                ext_arg = (ext_arg | co_code[offset + 1]) << 8
                offset += 2
                continue
            line = byte_to_line.get(offset)
            if line is not None:
                line = multiline.get(line, line)
                if line and line != from_line:
                    out.add((from_line, line))
                    break
            if op in jumps:
                if op in always:
                    arg = ext_arg | co_code[offset + 1]
                    following = offset + 2
                    while following < len(co_code) and co_code[following] == cache:
                        following += 2
                    offset = following - 2 * arg if op in backward else following + 2 * arg
                    ext_arg = 0
                    continue
                break
            if op in returns:
                out.add((from_line, -code.co_firstlineno))
                break
            if op in no_fall:
                break
            ext_arg = 0
            offset += 2
    return out


def _compile_globs(raw):
    """coverage.py ``GlobMatcher`` over already-prepped patterns (whole-path
    match), or None when there are none."""
    import json

    try:
        patterns = json.loads(raw) if raw else []
    except ValueError:
        patterns = []
    if not patterns:
        return None
    return _re.compile("(?:" + "|".join(_glob_to_regex(p) for p in patterns) + r")\Z")


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
    omit = _compile_globs(os.environ.get("PYTEST_RS_COV_OMIT"))
    include = _compile_globs(os.environ.get("PYTEST_RS_COV_INCLUDE"))

    mon = sys.monitoring
    tool = mon.COVERAGE_ID
    try:
        mon.use_tool_id(tool, "pytest-rs-cov-child")
    except ValueError:
        return  # someone else measures this process

    hits = {}
    # (id(code), source offset) -> destination offsets; resolved at exit.
    branches = {}
    tracked = {}  # id(code) -> (filename, code)
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

    def is_tracked(filename):
        if filename.startswith("<") or "site-packages" in filename or "/lib/python" in filename:
            return False
        if omit is not None or include is not None:
            # coverage.py matches canonical file names.
            names = (filename, os.path.realpath(filename))
            if omit is not None and any(omit.match(n) for n in names):
                return False
            if include is not None:
                return any(include.match(n) for n in names)
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
            # `rootdir/.` on sys.path leaves `/./` in file names; coverage.py
            # normalizes them.
            filename = os.path.normpath(code.co_filename)
            if code.co_name != "__annotate__" and is_tracked(filename):
                tracked[key] = (_map_alias(filename), code)
                mon.set_local_events(tool, code, local_events)
        return mon.DISABLE

    def line(code, lineno):
        entry = tracked.get(id(code))
        if entry is not None:
            hits.setdefault(entry[0], set()).add(lineno)
        return mon.DISABLE

    def record(code, src_offset, dst_offset):
        if id(code) not in tracked:
            return 0
        dests = branches.setdefault((id(code), src_offset), set())
        dests.add(dst_offset)
        return len(dests)

    def branch_left(code, src_offset, dst_offset):
        record(code, src_offset, dst_offset)
        return mon.DISABLE

    def branch_right(code, src_offset, dst_offset):
        record(code, src_offset, dst_offset)
        return mon.DISABLE

    def branch_compat(code, src_offset, dst_offset):
        if record(code, src_offset, dst_offset) >= 2:
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
        if not hits and not branches:
            return
        arcs = {}
        by_code = {}
        for (key, source), dests in branches.items():
            by_code.setdefault(key, []).extend((source, dest) for dest in dests)
        multiline_maps = {}
        for key, pairs in by_code.items():
            filename, code = tracked[key]
            multiline = multiline_maps.get(filename)
            if multiline is None:
                multiline = multiline_maps[filename] = _multiline_map(code.co_filename)
            arcs.setdefault(filename, set()).update(_resolve_arcs(code, pairs, multiline))
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
