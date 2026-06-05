"""Assertion rewriting: a sys.meta_path hook that rewrites `assert`
statements in test modules so failures show evaluated values.

This is a simplified port of pytest's AssertionRewriter working on the
CPython ast (locations copied node-by-node, so tracebacks stay exact).
"""

import ast
import importlib.machinery
import importlib.util
import os
import sys

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
_REGISTERED_MODULES = set()


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


def _explain_eq(left, right):
    """Extra explanation lines for a failed `==`, matching what pytest's
    assertion plugin appends (iterable diff via _compare_eq_iterable)."""
    try:
        if isinstance(left, (str, bytes)) or isinstance(right, (str, bytes)):
            return ""
        if not (hasattr(left, "__iter__") and hasattr(right, "__iter__")):
            return ""
        from _pytest.assertion.util import _compare_eq_any

        lines = _compare_eq_any(left, right, lambda text, *args, **kwargs: text, 0)
        if not lines:
            return ""
        return "\n  " + "\n  ".join(lines)
    except Exception:
        return ""


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
                PytestAssertRewriteWarning(
                    "assertion is always true, perhaps remove parentheses?"
                ),
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
        return [node.msg, ast.Constant("\n")]

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

        def repr_of(name):
            return ast.FormattedValue(
                value=ast.Name(id=name, ctx=ast.Load()), conversion=ord("r"), format_spec=None
            )

        values = [
            *self._user_msg_prefix(node),
            ast.Constant("assert "),
            repr_of(left_name),
            ast.Constant(f" {op} "),
            repr_of(right_name),
        ]
        if isinstance(test.ops[0], ast.Eq):
            # pytest parity: a failed == on iterables appends diff lines.
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
                    attr="_explain_eq",
                    ctx=ast.Load(),
                ),
                args=[
                    ast.Name(id=left_name, ctx=ast.Load()),
                    ast.Name(id=right_name, ctx=ast.Load()),
                ],
                keywords=[],
            )
            values.append(ast.FormattedValue(value=explain, conversion=-1, format_spec=None))
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
        statements = [assign_left, assign_right, check]
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
        # Always compile from source, bypassing the bytecode cache:
        # CPython's pyc validation (mtime seconds + size) misses the
        # same-second same-size rewrites pytester does constantly.
        path = self.get_filename(fullname)
        return self.source_to_code(self.get_data(path), path)

    def source_to_code(self, data, path, *, _optimize=-1):
        tree = ast.parse(data, filename=path)
        _AssertRewriter(path).visit(tree)
        ast.fix_missing_locations(tree)
        return compile(tree, path, "exec", dont_inherit=True, optimize=_optimize)


class _RewriteFinder:
    def find_spec(self, name, path=None, target=None):
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
