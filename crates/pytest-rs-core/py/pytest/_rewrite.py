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
_CACHE_VERSION = 4
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

# Extra python_files glob patterns beyond the default test_*.py / *_test.py,
# registered by the engine when it reads the ini config.
_PYTHON_FILES_GLOBS: set = set()


def register_assert_rewrite(*names):
    _REGISTERED_MODULES.update(names)


def register_python_files_globs(patterns):
    """Register extra python_files glob patterns for assertion rewriting.

    Called by the engine after reading ini so that non-standard test file
    patterns (e.g. 'testing/python/*.py') are also assertion-rewritten.
    ``patterns`` is an iterable of glob strings.
    """
    _PYTHON_FILES_GLOBS.update(patterns)


def _is_registered_module(name):
    return any(
        name == registered or name.startswith(registered + ".")
        for registered in _REGISTERED_MODULES
    )


def _is_rewrite_target(origin):
    import fnmatch as _fnmatch

    basename = os.path.basename(origin)
    if basename == "conftest.py":
        return True
    if not basename.endswith(".py"):
        return False
    if basename.startswith("test_") or basename.endswith("_test.py"):
        return True
    # Check against any extra python_files patterns registered by the engine.
    for pat in _PYTHON_FILES_GLOBS:
        # Bare filename glob (e.g. "test_*.py", "*.py"): match against basename.
        if os.sep not in pat and "/" not in pat:
            if _fnmatch.fnmatch(basename, pat):
                return True
        else:
            # Path-style glob (e.g. "testing/python/*.py"): normalize separators
            # and match the origin's suffix.  We normalise to forward slashes and
            # check that the origin (also forward-slash-normalised) ends with the
            # pattern prefix resolved as a suffix.
            norm_pat = pat.replace(os.sep, "/")
            norm_origin = origin.replace(os.sep, "/")
            if _fnmatch.fnmatch(norm_origin, "*/" + norm_pat):
                return True
            # Also try a direct match against the full origin in case it's an
            # absolute pattern.
            if _fnmatch.fnmatch(norm_origin, norm_pat):
                return True
    return False


# -v/-vv level, set by the engine at startup: full iterable diffs need
# verbose >= 1, "Omitting N identical items" folds below 2 (pytest gates
# its explanations on config verbosity the same way).
_verbosity = 0
_assertion_verbosity = None

# truncation_limit_lines / truncation_limit_chars ini values, set by the
# engine at startup (None means "use pytest's defaults": 8 lines, 640 chars).
_truncation_lines = None
_truncation_chars = None

# --assert=plain disables rewriting entirely (failed asserts surface as bare
# AssertionError); the engine flips this from config before collection.
_rewrite_enabled = True


def set_verbosity(level, assertion_level=None):
    global _verbosity, _assertion_verbosity
    _verbosity = level
    _assertion_verbosity = assertion_level


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
        if verbosity_type == "assertions" and _assertion_verbosity is not None:
            return _assertion_verbosity
        return _verbosity

    def get_terminal_writer(self):
        return self

    @property
    def hasmarkup(self):
        from pytest import _tb

        return _tb._color

    @property
    def code_highlight(self):
        return self.hasmarkup

    def _highlight(self, source, lexer="python"):
        if not source or not self.hasmarkup:
            return source
        try:
            import os

            from pygments import highlight as pygments_highlight
            from pygments.formatters.terminal import TerminalFormatter

            if lexer == "diff":
                from pygments.lexers.diff import DiffLexer

                pygments_lexer = DiffLexer()
            else:
                from pygments.lexers.python import PythonLexer

                pygments_lexer = PythonLexer()

            mode = os.getenv("PYTEST_THEME_MODE", "dark")
            style = os.getenv("PYTEST_THEME")
            highlighted = pygments_highlight(
                source, pygments_lexer, TerminalFormatter(bg=mode, style=style)
            )
            if highlighted.endswith("\n") and not source.endswith("\n"):
                highlighted = highlighted[:-1]
            return "\x1b[0m" + highlighted
        except Exception:
            return source


def _format_assert(op, left, right):
    """The full AssertionError text after "assert " for a failed comparison:
    pytest's assertrepr_compare explanation (saferepr'd summary + op-specific
    diff lines), truncated like pytest's callbinrepr unless -vv/CI/ini opt
    out."""
    try:
        from _pytest._io.saferepr import saferepr, saferepr_unlimited

        av = _assertion_verbosity if _assertion_verbosity is not None else _verbosity
        _repr = saferepr_unlimited if av >= 2 else saferepr
        fallback = f"{_repr(left)} {op} {_repr(right)}"
    except Exception:
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
        assert_verbose = _assertion_verbosity if _assertion_verbosity is not None else _verbosity
        should_truncate = (
            assert_verbose < 2 and not running_on_ci() and (max_lines > 0 or max_chars > 0)
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

    def _decompose_expr(self, expr, loc):
        """Recursively decompose Call arguments into temp-var assignments.

        For each Call node, non-trivial arguments (and keyword values) are
        extracted into their own temp assignments so that intermediate objects
        stay alive across the whole assertion.  This mirrors real pytest's
        behaviour and matters when object identity (and thus id()-based
        __hash__) is relevant: without decomposition, a temporary created for
        one argument can be freed before the next temporary is allocated,
        allowing CPython to reuse the same memory address.

        Returns (stmts, new_expr) where stmts is a (possibly empty) list of
        ast.Assign nodes that must be emitted *before* the expression that
        uses new_expr.
        """
        _SIMPLE = (ast.Name, ast.Constant, ast.Attribute, ast.Subscript)
        if not isinstance(expr, ast.Call):
            return [], expr

        stmts = []

        # Recursively decompose each positional argument.
        new_args = []
        for arg in expr.args:
            # Starred (*args) cannot be assigned to a temp var.
            if isinstance(arg, ast.Starred):
                new_args.append(arg)
                continue
            sub_stmts, new_arg = self._decompose_expr(arg, loc)
            stmts.extend(sub_stmts)
            if isinstance(new_arg, _SIMPLE):
                new_args.append(new_arg)
            else:
                tmp = self._temp()
                assign = ast.Assign(
                    targets=[ast.Name(id=tmp, ctx=ast.Store())],
                    value=new_arg,
                )
                ast.copy_location(assign, loc)
                ast.copy_location(assign.targets[0], loc)
                stmts.append(assign)
                new_args.append(ast.copy_location(ast.Name(id=tmp, ctx=ast.Load()), loc))

        # Recursively decompose keyword values.
        new_keywords = []
        for kw in expr.keywords:
            sub_stmts, new_val = self._decompose_expr(kw.value, loc)
            stmts.extend(sub_stmts)
            if isinstance(new_val, _SIMPLE):
                new_keywords.append(ast.keyword(arg=kw.arg, value=new_val))
            else:
                tmp = self._temp()
                assign = ast.Assign(
                    targets=[ast.Name(id=tmp, ctx=ast.Store())],
                    value=new_val,
                )
                ast.copy_location(assign, loc)
                ast.copy_location(assign.targets[0], loc)
                stmts.append(assign)
                new_keywords.append(
                    ast.keyword(
                        arg=kw.arg, value=ast.copy_location(ast.Name(id=tmp, ctx=ast.Load()), loc)
                    )
                )

        new_expr = ast.copy_location(
            ast.Call(func=expr.func, args=new_args, keywords=new_keywords), expr
        )
        return stmts, new_expr

    def _rewrite_compare(self, node):
        test = node.test
        op = _OPS.get(type(test.ops[0]))
        if op is None:
            return self._rewrite_generic(node)

        # Decompose sub-expressions so intermediate objects stay alive.
        left_decomp_stmts, left_expr = self._decompose_expr(test.left, node)
        right_decomp_stmts, right_expr = self._decompose_expr(test.comparators[0], node)

        left_name = self._temp()
        right_name = self._temp()
        assign_left = ast.Assign(targets=[ast.Name(id=left_name, ctx=ast.Store())], value=left_expr)
        assign_right = ast.Assign(
            targets=[ast.Name(id=right_name, ctx=ast.Store())],
            value=right_expr,
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
        _TRIVIAL = (
            ast.Name,
            ast.Constant,
            ast.Dict,
            ast.List,
            ast.Set,
            ast.Tuple,
            ast.JoinedStr,
            ast.FormattedValue,
            ast.Subscript,
            ast.Attribute,
        )

        def _is_approx_call(node):
            """Return True if node is approx(...) or pytest.approx(...).
            These objects carry their own rich repr and _repr_compare output,
            so a redundant '+  where approx(...) = approx(...)' line is noise
            (the real pytest assertion rewriter never emits it either)."""
            if not isinstance(node, ast.Call):
                return False
            func = node.func
            # approx(...)
            if isinstance(func, ast.Name) and func.id == "approx":
                return True
            # pytest.approx(...)
            if (
                isinstance(func, ast.Attribute)
                and func.attr == "approx"
                and isinstance(func.value, ast.Name)
                and func.value.id == "pytest"
            ):
                return True
            return False

        left_trivial = isinstance(test.left, _TRIVIAL) or _is_approx_call(test.left)
        right_trivial = isinstance(test.comparators[0], _TRIVIAL) or _is_approx_call(
            test.comparators[0]
        )
        if not left_trivial:
            try:
                src = ast.unparse(test.left)
            except Exception:
                src = None
            if src:
                values.extend(
                    [
                        ast.Constant("\n +  where "),
                        ast.FormattedValue(
                            value=ast.Name(id=left_name, ctx=ast.Load()),
                            conversion=114,
                            format_spec=None,
                        ),
                        ast.Constant(f" = {src}"),
                    ]
                )
        if not right_trivial:
            try:
                src = ast.unparse(test.comparators[0])
            except Exception:
                src = None
            if src:
                label = "and   " if not left_trivial else "where "
                values.extend(
                    [
                        ast.Constant(f"\n +  {label}"),
                        ast.FormattedValue(
                            value=ast.Name(id=right_name, ctx=ast.Load()),
                            conversion=114,
                            format_spec=None,
                        ),
                        ast.Constant(f" = {src}"),
                    ]
                )
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
        # Include any temp vars from sub-expression decomposition.
        decomp_tmp_names = [
            assign.targets[0].id for assign in left_decomp_stmts + right_decomp_stmts
        ]
        all_tmp_names = decomp_tmp_names + [left_name, right_name]
        clear = ast.Delete(targets=[ast.Name(id=n, ctx=ast.Del()) for n in all_tmp_names])
        statements = [
            *left_decomp_stmts,
            *right_decomp_stmts,
            assign_left,
            assign_right,
            check,
            clear,
        ]
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
            _builtins_import = ast.Import(names=[ast.alias(name="builtins", asname="@py_builtins")])
            insert_pos = 0
            for i, stmt in enumerate(tree.body):
                if isinstance(stmt, ast.ImportFrom) and stmt.module == "__future__":
                    insert_pos = i + 1
                elif (
                    isinstance(stmt, ast.Expr)
                    and isinstance(stmt.value, ast.Constant)
                    and isinstance(stmt.value.value, str)
                ):
                    if insert_pos <= i:
                        insert_pos = i + 1
                else:
                    break
            tree.body.insert(insert_pos, _builtins_import)
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

    def mark_rewrite(self, *names):
        _REGISTERED_MODULES.update(names)


_FINDER = _RewriteFinder()


def install():
    if not any(isinstance(finder, _RewriteFinder) for finder in sys.meta_path):
        sys.meta_path.insert(0, _FINDER)
