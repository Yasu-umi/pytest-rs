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
_CACHE_VERSION = 8
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

# Symbols for the recursive "where" explanation builder (_AssertRewriter
# .decompose_and_explain), matching upstream's BINOP_MAP/UNARY_MAP
# (_pytest.assertion.rewrite).
_BINOP_SYMS = {
    ast.BitOr: "|",
    ast.BitXor: "^",
    ast.BitAnd: "&",
    ast.LShift: "<<",
    ast.RShift: ">>",
    ast.Add: "+",
    ast.Sub: "-",
    ast.Mult: "*",
    ast.Div: "/",
    ast.FloorDiv: "//",
    ast.Mod: "%",
    ast.Pow: "**",
    ast.MatMult: "@",
}
_UNARY_PREFIXES = {ast.Not: "not ", ast.Invert: "~", ast.USub: "-", ast.UAdd: "+"}


# Module names (and their submodules) explicitly opted into rewriting via
# pytest.register_assert_rewrite, e.g. bundled plugin shims like pytest_mock.
_REGISTERED_MODULES: set = set()

# Extra python_files glob patterns beyond the default test_*.py / *_test.py,
# registered by the engine when it reads the ini config.
_PYTHON_FILES_GLOBS: set = set()


def register_assert_rewrite(*names):
    # pytest-rs has its own rewriter (_RewriteLoader) that hooks into the
    # import machinery via sys.meta_path; no need to warn about already-imported
    # modules since rewriting is handled transparently at import time.
    _REGISTERED_MODULES.update(names)


def _warn_already_imported(name: str) -> None:
    """Warn when a module is registered for rewriting after it was already imported."""
    from pytest._warning_types import PytestAssertRewriteWarning
    from pytest._wcapture import _fire_config_time_warning

    warning = PytestAssertRewriteWarning(f"Module already imported so cannot be rewritten; {name}")
    # stacklevel=3: _fire_config_time_warning(0) → _warn_already_imported(1)
    # → register_assert_rewrite(2) → caller in conftest/test(3)
    _fire_config_time_warning(warning, stacklevel=3)


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
            import pathlib

            from pytest import _wcapture
            from pytest._pluginmanager import pluginmanager

            # Scope conftest-registered impls to the running test's directory
            # (upstream: item.ihook is a gethookproxy subset, not the global
            # hook) — a sibling directory's conftest must not apply here.
            exclude = set()
            test_path = _wcapture.current_test_path
            if test_path is not None:
                in_scope = set(pluginmanager._getconftestmodules(pathlib.Path(test_path)))
                exclude = pluginmanager._conftest_plugins - in_scope
            hooked = pluginmanager.hook.pytest_assertrepr_compare.call_excluding(
                exclude, config=cfg, op=op, left=left, right=right
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


def _format_where(expl_template):
    """Render a "{"/"}"-marked explanation template (see
    _AssertRewriter._wrap_block) into the indented "+  where"/"+    where"/
    "+  and" lines pytest shows after a failed comparison's summary. Reuses
    upstream's own _pytest.assertion.util.format_explanation (bundled
    as-is) for the nesting/sibling ("where" vs "and") bookkeeping instead of
    re-deriving it here, since it already implements this exactly.

    `expl_template` starts with an empty anchor line (see _wrap_block's
    caller) so the result always starts with "\\n", ready to append
    directly after the "assert ..." summary line."""
    try:
        from _pytest.assertion import util
    except Exception:
        return ""
    return util.format_explanation(expl_template)


def _should_repr_global_name(obj):
    """Port of upstream's visit_Name gate: a bare global Name is shown by
    repr only if it isn't a callable/module/class-like object (those read
    better as their plain identifier, e.g. `assert some_function`)."""
    if callable(obj):
        return False
    try:
        return not hasattr(obj, "__name__")
    except Exception:
        return True


def _saferepr_or_repr(obj):
    """Port of upstream's `_saferepr` (_pytest.assertion.rewrite): bound
    methods show just their name (dropping the redundant "<bound method
    ...>" noise), the repr is verbosity-aware truncated (full text at -vv,
    10x the default limit at -v), and embedded newlines are escaped — the
    text is often embedded into _pytest.assertion.util.format_explanation's
    "{"/"}" mini-language, where raw newlines are significant."""
    import types

    if isinstance(obj, types.MethodType):
        try:
            return obj.__name__
        except Exception:
            pass
    av = _assertion_verbosity if _assertion_verbosity is not None else _verbosity
    try:
        from _pytest._io.saferepr import DEFAULT_REPR_MAX_SIZE, saferepr, saferepr_unlimited

        if av >= 2:
            text = saferepr_unlimited(obj)
        elif av >= 1:
            text = saferepr(obj, maxsize=DEFAULT_REPR_MAX_SIZE * 10)
        else:
            text = saferepr(obj)
    except Exception:
        try:
            text = repr(obj)
        except Exception:
            text = object.__repr__(obj)
    return text.replace("\n", "\\n")


class _AssertRewriter(ast.NodeTransformer):
    def __init__(self, path="<unknown>"):
        self._counter = 0
        self._path = path

    def _temp(self):
        self._counter += 1
        return f"@pytest_rs_tmp{self._counter}"

    @staticmethod
    def _module_helper_call(func_name, args):
        """AST for `__import__("pytest._rewrite")._rewrite.<func_name>(*args)`."""
        return ast.Call(
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
                attr=func_name,
                ctx=ast.Load(),
            ),
            args=args,
            keywords=[],
        )

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

    @staticmethod
    def _is_approx_call(call):
        """Return True if node is approx(...) or pytest.approx(...).
        These objects carry their own rich repr and _repr_compare output,
        so a redundant '+  where approx(...) = approx(...)' line is noise
        (the real pytest assertion rewriter never emits it either)."""
        if not isinstance(call, ast.Call):
            return False
        func = call.func
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

    def _name_repr_ifexp(self, name):
        """AST for `<repr> if <local-or-should-repr> else "<bare-id>"`,
        mirroring upstream's visit_Name gate (see _should_repr_global_name)."""
        locs_call = ast.Call(func=ast.Name(id="locals", ctx=ast.Load()), args=[], keywords=[])
        inlocs = ast.Compare(left=ast.Constant(name.id), ops=[ast.In()], comparators=[locs_call])
        dorepr = self._module_helper_call(
            "_should_repr_global_name", [ast.Name(id=name.id, ctx=ast.Load())]
        )
        test_expr = ast.BoolOp(op=ast.Or(), values=[inlocs, dorepr])
        display_call = self._module_helper_call(
            "_saferepr_or_repr", [ast.Name(id=name.id, ctx=ast.Load())]
        )
        return ast.IfExp(test=test_expr, body=display_call, orelse=ast.Constant(name.id))

    def _repr_formatted(self, ref):
        """A JoinedStr value piece rendering `_saferepr_or_repr(ref)`."""
        call = self._module_helper_call("_saferepr_or_repr", [ref])
        return ast.FormattedValue(value=call, conversion=-1, format_spec=None)

    @staticmethod
    def _mk_assign(name, value, loc):
        assign = ast.Assign(targets=[ast.Name(id=name, ctx=ast.Store())], value=value)
        ast.copy_location(assign, loc)
        ast.copy_location(assign.targets[0], loc)
        return assign

    def _wrap_block(self, value_ref, desc_parts):
        """Just the "{value = desc}" mini-language block itself (see
        _pytest.assertion.util.format_explanation and _format_where above)
        — no leading inline repr, since the caller decides separately
        whether/where a repr of value_ref also needs to appear inline (see
        _child_display, which needs both; the top-level compare operand,
        which doesn't — its value already appears in the "assert x == y"
        summary line)."""
        return [
            ast.Constant("\n{"),
            self._repr_formatted(value_ref),
            ast.Constant(" = "),
            *desc_parts,
            ast.Constant("\n}"),
        ]

    def _child_display(self, expr, stmts):
        """Decompose `expr` (a sub-node reached while explaining a parent
        expression) and return (value_ref, display_parts): display_parts is
        an inline repr followed by its own "{...}" block for Call/Attribute
        (matching upstream — they get their own "where" line wherever
        referenced), or left as bare inline text for everything else
        (BinOp/UnaryOp/Name/Constant/fallback fold into the parent's own
        text, unwrapped)."""
        value_ref, desc_parts = self._decompose_and_explain(expr, stmts)
        if isinstance(expr, (ast.Attribute, ast.Call)) and not self._is_approx_call(expr):
            return value_ref, [
                self._repr_formatted(value_ref),
                *self._wrap_block(value_ref, desc_parts),
            ]
        return value_ref, desc_parts

    def _decompose_and_explain(self, expr, stmts):
        """Recursively single-evaluate `expr` into temp-var assignments
        (Python's actual evaluation order — matters for object identity and
        side effects) and return (value_ref, desc_parts): value_ref is an
        ast.expr referencing the already-computed value (a temp Name for
        anything with possible side effects, or the original node for
        side-effect-free Name-Load / Constant), and desc_parts is a list of
        JoinedStr value pieces (Constant / FormattedValue) describing how
        that value was derived — an adapted, recursive port of upstream's
        visit_Name/visit_Attribute/visit_Call/visit_BinOp/visit_UnaryOp
        (_pytest.assertion.rewrite). Appends the necessary ast.Assign
        statements, in evaluation order, to `stmts`.

        Node types outside this function's scope (Subscript, BoolOp,
        comprehensions, lambdas, walrus, chained Compare, ...) still get a
        single-eval temp for correctness, but no recursive breakdown — just
        their own repr (or unparsed source, when available) is shown,
        matching the previous implementation's fallback for these.
        """
        if isinstance(expr, ast.Constant):
            return expr, [ast.Constant(repr(expr.value))]

        if isinstance(expr, ast.Name) and isinstance(expr.ctx, ast.Load):
            return expr, [
                ast.FormattedValue(
                    value=self._name_repr_ifexp(expr), conversion=-1, format_spec=None
                )
            ]

        if isinstance(expr, ast.Attribute) and isinstance(expr.ctx, ast.Load):
            base_ref, base_desc = self._child_display(expr.value, stmts)
            tmp = self._temp()
            access = ast.Attribute(value=base_ref, attr=expr.attr, ctx=ast.Load())
            stmts.append(self._mk_assign(tmp, access, expr))
            return ast.Name(id=tmp, ctx=ast.Load()), [*base_desc, ast.Constant(f".{expr.attr}")]

        if isinstance(expr, ast.Call):
            func_ref, func_desc = self._child_display(expr.func, stmts)
            pos_refs = []
            kw_refs = []
            desc_groups = []
            for arg in expr.args:
                if isinstance(arg, ast.Starred):
                    inner_ref, inner_desc = self._child_display(arg.value, stmts)
                    pos_refs.append(ast.Starred(value=inner_ref, ctx=ast.Load()))
                    desc_groups.append([ast.Constant("*"), *inner_desc])
                    continue
                ref, desc = self._child_display(arg, stmts)
                pos_refs.append(ref)
                desc_groups.append(desc)
            for kw in expr.keywords:
                ref, desc = self._child_display(kw.value, stmts)
                kw_refs.append(ast.keyword(arg=kw.arg, value=ref))
                if kw.arg:
                    desc_groups.append([ast.Constant(f"{kw.arg}="), *desc])
                else:
                    desc_groups.append([ast.Constant("**"), *desc])
            tmp = self._temp()
            new_call = ast.Call(func=func_ref, args=pos_refs, keywords=kw_refs)
            stmts.append(self._mk_assign(tmp, new_call, expr))
            desc = [*func_desc, ast.Constant("(")]
            for i, group in enumerate(desc_groups):
                if i:
                    desc.append(ast.Constant(", "))
                desc.extend(group)
            desc.append(ast.Constant(")"))
            return ast.Name(id=tmp, ctx=ast.Load()), desc

        if isinstance(expr, ast.BinOp) and type(expr.op) in _BINOP_SYMS:
            left_ref, left_desc = self._child_display(expr.left, stmts)
            right_ref, right_desc = self._child_display(expr.right, stmts)
            tmp = self._temp()
            new_binop = ast.BinOp(left=left_ref, op=expr.op, right=right_ref)
            stmts.append(self._mk_assign(tmp, new_binop, expr))
            sym = _BINOP_SYMS[type(expr.op)]
            desc = [
                ast.Constant("("),
                *left_desc,
                ast.Constant(f" {sym} "),
                *right_desc,
                ast.Constant(")"),
            ]
            return ast.Name(id=tmp, ctx=ast.Load()), desc

        if isinstance(expr, ast.UnaryOp) and type(expr.op) in _UNARY_PREFIXES:
            operand_ref, operand_desc = self._child_display(expr.operand, stmts)
            tmp = self._temp()
            new_unary = ast.UnaryOp(op=expr.op, operand=operand_ref)
            stmts.append(self._mk_assign(tmp, new_unary, expr))
            desc = [ast.Constant(_UNARY_PREFIXES[type(expr.op)]), *operand_desc]
            return ast.Name(id=tmp, ctx=ast.Load()), desc

        if isinstance(expr, ast.NamedExpr) and isinstance(expr.target, ast.Name):
            # `(m := expr)`: evaluate and bind to the walrus target itself
            # (so a later plain reference to that name, e.g. in a
            # comma-separated assert message, sees the same value) and
            # describe it exactly like its assigned value — the walrus
            # wrapper itself carries no explanatory text of its own.
            value_ref, desc = self._decompose_and_explain(expr.value, stmts)
            assign = ast.Assign(
                targets=[ast.Name(id=expr.target.id, ctx=ast.Store())], value=value_ref
            )
            ast.copy_location(assign, expr)
            ast.copy_location(assign.targets[0], expr)
            stmts.append(assign)
            return ast.Name(id=expr.target.id, ctx=ast.Load()), desc

        # Fallback: node types outside this function's recursive coverage.
        # Still single-eval into a temp for correctness (object identity /
        # side effects), but only show its own repr or unparsed source.
        tmp = self._temp()
        stmts.append(self._mk_assign(tmp, expr, expr))
        ref = ast.Name(id=tmp, ctx=ast.Load())
        try:
            src = ast.unparse(expr)
        except Exception:
            src = None
        if src is None:
            return ref, [self._repr_formatted(ref)]
        return ref, [ast.Constant(src)]

    def _rewrite_compare(self, node):
        test = node.test
        op = _OPS.get(type(test.ops[0]))
        if op is None:
            return self._rewrite_generic(node)

        # Recursively single-evaluate both operands into temp-var
        # assignments, alongside a description of how each was derived.
        pre_stmts = []
        left_ref, left_desc = self._decompose_and_explain(test.left, pre_stmts)
        right_ref, right_desc = self._decompose_and_explain(test.comparators[0], pre_stmts)

        left_name = self._temp()
        right_name = self._temp()
        assign_left = ast.Assign(targets=[ast.Name(id=left_name, ctx=ast.Store())], value=left_ref)
        assign_right = ast.Assign(
            targets=[ast.Name(id=right_name, ctx=ast.Store())],
            value=right_ref,
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
        )

        left_trivial = isinstance(test.left, _TRIVIAL) or self._is_approx_call(test.left)
        right_trivial = isinstance(test.comparators[0], _TRIVIAL) or self._is_approx_call(
            test.comparators[0]
        )
        # Seed with an empty anchor line so _format_where's leading "line 0"
        # (discarded — it belongs to the "assert ..." summary above, not to
        # this suffix) never collides with a real where-block.
        where_values = [ast.Constant("")]
        if not left_trivial:
            where_values.extend(self._wrap_block(ast.Name(id=left_name, ctx=ast.Load()), left_desc))
        if not right_trivial:
            where_values.extend(
                self._wrap_block(ast.Name(id=right_name, ctx=ast.Load()), right_desc)
            )
        if len(where_values) > 1:
            values.append(
                ast.FormattedValue(
                    value=self._module_helper_call(
                        "_format_where", [ast.JoinedStr(values=where_values)]
                    ),
                    conversion=-1,
                    format_spec=None,
                )
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
        all_tmp_names = [assign.targets[0].id for assign in pre_stmts] + [left_name, right_name]
        clear = ast.Delete(targets=[ast.Name(id=n, ctx=ast.Del()) for n in all_tmp_names])
        statements = [
            *pre_stmts,
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
        """Keep the assert but add the source text to the message. A
        top-level Call/Attribute test instead shows its own runtime value
        (inline repr) plus a "where" breakdown of how it was derived —
        matching how _child_display treats a Call/Attribute reached while
        explaining a parent expression (upstream's real assertion rewriter
        explains a Call/Attribute's VALUE, not its static source text;
        e.g. `assert obj.method(arg)` shows "assert False", not the
        literal source, when the call returns a falsy value)."""
        test = node.test
        if isinstance(test, ast.Name) and isinstance(test.ctx, ast.Load):
            return self._rewrite_name(node, test)
        if isinstance(test, (ast.Call, ast.Attribute)) and not self._is_approx_call(test):
            return self._rewrite_call_or_attribute(node, test)
        try:
            source = ast.unparse(test)
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

    def _rewrite_call_or_attribute(self, node, test):
        """`assert obj.method(arg)` / `assert obj.attr`: show the runtime
        value's inline repr as the summary ("assert False") plus a "where"
        block decomposing how it was derived — the same treatment
        _child_display gives a Call/Attribute reached while explaining a
        parent expression, just applied at the top level."""
        pre_stmts = []
        value_ref, desc_parts = self._decompose_and_explain(test, pre_stmts)
        where_values = [ast.Constant(""), *self._wrap_block(value_ref, desc_parts)]
        values = [
            *self._user_msg_prefix(node),
            ast.Constant("assert "),
            self._repr_formatted(value_ref),
            ast.FormattedValue(
                value=self._module_helper_call(
                    "_format_where", [ast.JoinedStr(values=where_values)]
                ),
                conversion=-1,
                format_spec=None,
            ),
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
            test=ast.UnaryOp(op=ast.Not(), operand=value_ref),
            body=[raise_stmt],
            orelse=[],
        )
        clear_names = [assign.targets[0].id for assign in pre_stmts]
        statements = [*pre_stmts, check]
        if clear_names:
            statements.append(
                ast.Delete(targets=[ast.Name(id=n, ctx=ast.Del()) for n in clear_names])
            )
        for statement in statements:
            for child in ast.walk(statement):
                ast.copy_location(child, node)
        return statements

    def _rewrite_name(self, node, name):
        """`assert x` on a bare Name: pytest shows the name's runtime repr
        when it's a local var or a 'should-repr' global (see
        _should_repr_global_name), else the bare identifier text — mirrors
        upstream's visit_Name."""
        ifexp = self._name_repr_ifexp(name)

        message = ast.JoinedStr(
            values=[
                *self._user_msg_prefix(node),
                ast.Constant("assert "),
                ast.FormattedValue(value=ifexp, conversion=-1, format_spec=None),
            ]
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
            test=ast.UnaryOp(op=ast.Not(), operand=name),
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
            # Inject `@py_builtins = __import__("builtins")` after docstrings
            # and __future__ imports.  Lineno matches the first real statement
            # (same as upstream pytest) so coverage.py doesn't count it as an
            # extra statement.
            insert_pos = 0
            for i, stmt in enumerate(tree.body):
                if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Constant):
                    insert_pos = i + 1  # skip docstring
                elif isinstance(stmt, ast.ImportFrom) and stmt.module == "__future__":
                    insert_pos = i + 1  # skip future imports
                else:
                    break
            builtins_assign = ast.Assign(
                targets=[ast.Name(id="@py_builtins", ctx=ast.Store())],
                value=ast.Call(
                    func=ast.Name(id="__import__", ctx=ast.Load()),
                    args=[ast.Constant("builtins")],
                    keywords=[],
                ),
            )
            if insert_pos < len(tree.body):
                # Mirror upstream pytest: lineno = first real statement's lineno
                # so coverage.py doesn't count this as an extra statement.
                ast.copy_location(builtins_assign, tree.body[insert_pos])
            else:
                builtins_assign.lineno = 1
                builtins_assign.end_lineno = 1
                builtins_assign.col_offset = 0
                builtins_assign.end_col_offset = 0
            ast.fix_missing_locations(builtins_assign)
            tree.body.insert(insert_pos, builtins_assign)
            _AssertRewriter(path).visit(tree)
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
