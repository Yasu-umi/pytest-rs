"""Assertion rewriting: a sys.meta_path hook that rewrites `assert`
statements in test modules so failures show evaluated values.

This is a simplified port of pytest's AssertionRewriter working on the
CPython ast (locations copied node-by-node, so tracebacks stay exact).
"""

import ast
import importlib.machinery
import importlib.util
import marshal
import os
import sys
import types

# Rewritten bytecode is cached in __pycache__ under a pytest-rs-specific pyc
# tag, so it can never be confused with CPython's own (non-rewritten) bytecode
# for the same file. Bump _CACHE_VERSION whenever _AssertRewriter's output
# changes, to invalidate any stale rewritten pyc left on disk.
_CACHE_VERSION = 1
_PYC_TAIL = f".{sys.implementation.cache_tag}-pytestrs{_CACHE_VERSION}.pyc"

_OPS = {
    ast.Eq: "==",
    ast.NotEq: "!=",
    ast.Lt: "<",
    ast.LtE: "<=",
    ast.Gt: ">",
    ast.GtE: ">=",
    ast.In: "in",
    ast.NotIn: "not in",
    ast.Is: "is",
    ast.IsNot: "is not",
}


# Module names (and their submodules) explicitly opted into rewriting via
# pytest.register_assert_rewrite, e.g. bundled plugin shims like pytest_mock.
_REGISTERED_MODULES: set = set()


def register_assert_rewrite(*names):
    _REGISTERED_MODULES.update(names)


def _is_registered_module(name):
    return any(
        name == registered or name.startswith(registered + ".")
        for registered in _REGISTERED_MODULES
    )


def _is_rewrite_target(origin):
    basename = os.path.basename(origin)
    return basename == "conftest.py" or (
        basename.endswith(".py") and (basename.startswith("test_") or basename.endswith("_test.py"))
    )


# -v/-vv level, set by the engine at startup: full iterable diffs need
# verbose >= 1, "Omitting N identical items" folds below 2 (pytest gates
# its explanations on config verbosity the same way).
_verbosity = 0

# truncation_limit_lines / truncation_limit_chars ini values, set by the
# engine at startup (None means "use pytest's defaults": 8 lines, 640 chars).
_truncation_lines = None
_truncation_chars = None

# --assert=plain disables rewriting entirely (failed asserts surface as bare
# AssertionError); the engine flips this from config before collection.
_rewrite_enabled = True


def set_verbosity(level):
    global _verbosity
    _verbosity = level


def set_enabled(flag):
    global _rewrite_enabled
    _rewrite_enabled = bool(flag)


def set_truncation_limits(lines, chars):
    global _truncation_lines, _truncation_chars
    _truncation_lines = lines
    _truncation_chars = chars


class _RewriteConfig:
    """Just enough of pytest's Config for util.assertrepr_compare and
    pytest_assertrepr_compare plugins (e.g. pytest-icdiff reads
    config.get_terminal_writer().hasmarkup)."""

    def get_verbosity(self, verbosity_type=None):
        return _verbosity

    def get_terminal_writer(self):
        return self

    @property
    def hasmarkup(self):
        from pytest import _tb

        return _tb._color

    def _highlight(self, source, lexer="python"):
        return source


def _format_assert(op, left, right):
    """The full AssertionError text after "assert " for a failed comparison:
    pytest's assertrepr_compare explanation (saferepr'd summary + op-specific
    diff lines), truncated like pytest's callbinrepr unless -vv/CI/ini opt
    out."""
    fallback = f"{left!r} {op} {right!r}"
    try:
        from _pytest import outcomes
        from _pytest.assertion import truncate, util
        from _pytest.compat import running_on_ci
    except Exception:
        return fallback
    try:
        cfg = _RewriteConfig()
        # pytest_assertrepr_compare plugins (pytest-icdiff, pytest-clarity)
        # win over the built-in comparison; first non-None explanation is used.
        expl = None
        try:
            from pytest._pluginmanager import pluginmanager

            hooked = pluginmanager.hook.pytest_assertrepr_compare(
                config=cfg, op=op, left=left, right=right
            )
            for result in hooked or []:
                if result:
                    expl = list(result)
                    break
        except Exception:
            expl = None
        if not expl:
            expl = util.assertrepr_compare(cfg, op, left, right)
        if not expl:
            return fallback
        max_lines = int(
            _truncation_lines if _truncation_lines is not None else truncate.DEFAULT_MAX_LINES
        )
        max_chars = int(
            _truncation_chars if _truncation_chars is not None else truncate.DEFAULT_MAX_CHARS
        )
        should_truncate = (
            _verbosity < 2 and not running_on_ci() and (max_lines > 0 or max_chars > 0)
        )
        if should_truncate:
            expl = truncate._truncate_explanation(expl, max_lines=max_lines, max_chars=max_chars)
        expl = [line.replace("\n", "\\n") for line in expl]
        return expl[0] + "".join("\n  " + line for line in expl[1:])
    except outcomes.Exit:
        raise
    except Exception:
        return fallback


def _explain_op(op, left, right):
    """Backwards-compatible alias (modules rewritten by an older engine):
    explanation lines only, the summary was rendered by the caller."""
    explained = _format_assert(op, left, right)
    summary, newline, rest = explained.partition("\n")
    return newline + rest if rest else ""


def _explain_eq(left, right):
    """Backwards-compatible alias (modules rewritten by an older engine)."""
    return _explain_op("==", left, right)


class _AssertRewriter(ast.NodeTransformer):
    def __init__(self, path="<unknown>"):
        self._counter = 0
        self._path = path

    def _temp(self):
        self._counter += 1
        return f"@pytest_rs_tmp{self._counter}"

    def visit_Assert(self, node):
        test = node.test
        if isinstance(test, ast.Tuple) and test.elts:
            import warnings

            from pytest._warning_types import PytestAssertRewriteWarning

            warnings.warn_explicit(
                PytestAssertRewriteWarning("assertion is always true, perhaps remove parentheses?"),
                category=None,
                filename=self._path,
                lineno=node.lineno,
            )
        if isinstance(test, ast.Compare) and len(test.ops) == 1:
            return self._rewrite_compare(node)
        return self._rewrite_generic(node)

    def _user_msg_prefix(self, node):
        """Pieces for the AssertionError message ahead of the explanation."""
        if node.msg is None:
            return []
        # The msg can be any expression (int, tuple, custom object); a raw node
        # in a JoinedStr would TypeError at join, so format it like an f-string
        # interpolation (str()-equivalent), matching `assert x, msg` semantics.
        return [
            ast.FormattedValue(value=node.msg, conversion=-1, format_spec=None),
            ast.Constant("\n"),
        ]

    def _rewrite_compare(self, node):
        test = node.test
        op = _OPS.get(type(test.ops[0]))
        if op is None:
            return self._rewrite_generic(node)

        left_name = self._temp()
        right_name = self._temp()
        assign_left = ast.Assign(targets=[ast.Name(id=left_name, ctx=ast.Store())], value=test.left)
        assign_right = ast.Assign(
            targets=[ast.Name(id=right_name, ctx=ast.Store())],
            value=test.comparators[0],
        )
        recomposed = ast.Compare(
            left=ast.Name(id=left_name, ctx=ast.Load()),
            ops=test.ops,
            comparators=[ast.Name(id=right_name, ctx=ast.Load())],
        )

        # pytest parity: a failed comparison renders assertrepr_compare's
        # explanation (saferepr'd summary, op-specific diff lines, runtime
        # truncation), all composed by the engine's runtime helper.
        explain = ast.Call(
            func=ast.Attribute(
                value=ast.Attribute(
                    value=ast.Call(
                        func=ast.Name(id="__import__", ctx=ast.Load()),
                        args=[ast.Constant("pytest._rewrite")],
                        keywords=[],
                    ),
                    attr="_rewrite",
                    ctx=ast.Load(),
                ),
                attr="_format_assert",
                ctx=ast.Load(),
            ),
            args=[
                ast.Constant(op),
                ast.Name(id=left_name, ctx=ast.Load()),
                ast.Name(id=right_name, ctx=ast.Load()),
            ],
            keywords=[],
        )
        values = [
            *self._user_msg_prefix(node),
            ast.Constant("assert "),
            ast.FormattedValue(value=explain, conversion=-1, format_spec=None),
        ]
        message = ast.JoinedStr(values=values)
        raise_stmt = ast.Raise(
            exc=ast.Call(
                func=ast.Name(id="AssertionError", ctx=ast.Load()),
                args=[message],
                keywords=[],
            ),
            cause=None,
        )
        check = ast.If(
            test=ast.UnaryOp(op=ast.Not(), operand=recomposed),
            body=[raise_stmt],
            orelse=[],
        )
        # Upstream parity: clear the temporaries after a passing assert —
        # they live in the frame, and leak tests (weakref + gc.collect
        # inside the test) would see the asserted values kept alive.
        clear = ast.Delete(
            targets=[
                ast.Name(id=left_name, ctx=ast.Del()),
                ast.Name(id=right_name, ctx=ast.Del()),
            ]
        )
        statements = [assign_left, assign_right, check, clear]
        for statement in statements:
            for child in ast.walk(statement):
                ast.copy_location(child, node)
        return statements

    def _rewrite_generic(self, node):
        """Keep the assert but add the source text to the message."""
        try:
            source = ast.unparse(node.test)
        except Exception:
            return node
        message = ast.JoinedStr(
            values=[*self._user_msg_prefix(node), ast.Constant(f"assert {source}")]
        )
        raise_stmt = ast.Raise(
            exc=ast.Call(
                func=ast.Name(id="AssertionError", ctx=ast.Load()),
                args=[message],
                keywords=[],
            ),
            cause=None,
        )
        check = ast.If(
            test=ast.UnaryOp(op=ast.Not(), operand=node.test),
            body=[raise_stmt],
            orelse=[],
        )
        for child in ast.walk(check):
            ast.copy_location(child, node)
        return check


class _RewriteLoader(importlib.machinery.SourceFileLoader):
    def get_code(self, fullname):
        # Cache rewritten bytecode in __pycache__ keyed on a *content hash*
        # (PEP 552 checked-hash pyc), not mtime+size. The hash is what lets us
        # cache where upstream's mtime-second granularity can't: it stays
        # correct under pytester's same-second, same-size in-place rewrites
        # (different content -> different hash -> recompile) while giving warm
        # runs the rewritten-bytecode reuse that uncached compilation forgoes.
        path = self.get_filename(fullname)
        source = self.get_data(path)
        cache_path = self._cache_path(path)
        code = self._read_pyc(source, cache_path)
        if code is not None:
            return code
        code = self.source_to_code(source, path)
        if not sys.dont_write_bytecode:
            self._write_pyc(code, source, cache_path)
        return code

    @staticmethod
    def _cache_path(source_path):
        head, tail = os.path.split(source_path)
        return os.path.join(head, "__pycache__", tail[: -len(".py")] + _PYC_TAIL)

    @staticmethod
    def _read_pyc(source, cache_path):
        """Return the cached code object iff the pyc's embedded source hash
        matches `source`; otherwise None. The pytest-rs tag + CPython magic
        guard means a stale interpreter or non-rewritten pyc is never reused."""
        try:
            with open(cache_path, "rb") as fp:
                data = fp.read()
        except OSError:
            return None
        if len(data) < 16 or data[:4] != importlib.util.MAGIC_NUMBER:
            return None
        if int.from_bytes(data[4:8], "little") != 0b11:  # hash-based, checked
            return None
        if data[8:16] != importlib.util.source_hash(source):
            return None
        try:
            code = marshal.loads(data[16:])
        except Exception:
            return None
        return code if isinstance(code, types.CodeType) else None

    @staticmethod
    def _write_pyc(code, source, cache_path):
        header = bytearray(importlib.util.MAGIC_NUMBER)
        header += (0b11).to_bytes(4, "little")  # hash-based + check_source flags
        header += importlib.util.source_hash(source)  # 8 bytes
        try:
            os.makedirs(os.path.dirname(cache_path), exist_ok=True)
            # Write to a process-unique temp then atomically rename, so parallel
            # workers sharing a __pycache__ never observe a half-written pyc.
            tmp = f"{cache_path}.{os.getpid()}"
            with open(tmp, "wb") as fp:
                fp.write(header)
                fp.write(marshal.dumps(code))
            os.replace(tmp, cache_path)
        except OSError:
            pass  # read-only dir etc.: skip caching, like upstream

    def source_to_code(self, data, path, *, _optimize=-1):
        tree = ast.parse(data, filename=path)
        # Upstream opt-out: a module docstring containing PYTEST_DONT_REWRITE
        # compiles verbatim.
        try:
            docstring = ast.get_docstring(tree, clean=False)
        except Exception:
            docstring = None
        if not (docstring and "PYTEST_DONT_REWRITE" in docstring):
            _AssertRewriter(path).visit(tree)
            ast.fix_missing_locations(tree)
        return compile(tree, path, "exec", dont_inherit=True, optimize=_optimize)


class _RewriteFinder:
    def find_spec(self, name, path=None, target=None):
        if not _rewrite_enabled:
            return None
        spec = importlib.machinery.PathFinder.find_spec(name, path, target)
        if (
            spec is not None
            and spec.origin is not None
            and spec.origin.endswith(".py")
            and (_is_rewrite_target(spec.origin) or _is_registered_module(name))
        ):
            spec.loader = _RewriteLoader(spec.name, spec.origin)
            return spec
        # Let the default machinery handle everything else.
        return None


_FINDER = _RewriteFinder()


def install():
    if not any(isinstance(finder, _RewriteFinder) for finder in sys.meta_path):
        sys.meta_path.insert(0, _FINDER)
