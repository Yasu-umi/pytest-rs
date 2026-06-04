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


def _is_rewrite_target(origin):
    basename = os.path.basename(origin)
    return basename == "conftest.py" or (
        basename.endswith(".py") and (basename.startswith("test_") or basename.endswith("_test.py"))
    )


class _AssertRewriter(ast.NodeTransformer):
    def __init__(self):
        self._counter = 0

    def _temp(self):
        self._counter += 1
        return f"@pytest_rs_tmp{self._counter}"

    def visit_Assert(self, node):
        test = node.test
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

        message = ast.JoinedStr(
            values=[
                *self._user_msg_prefix(node),
                ast.Constant("assert "),
                repr_of(left_name),
                ast.Constant(f" {op} "),
                repr_of(right_name),
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
    def source_to_code(self, data, path, *, _optimize=-1):
        tree = ast.parse(data, filename=path)
        _AssertRewriter().visit(tree)
        ast.fix_missing_locations(tree)
        return compile(tree, path, "exec", dont_inherit=True, optimize=_optimize)


class _RewriteFinder:
    def find_spec(self, name, path=None, target=None):
        spec = importlib.machinery.PathFinder.find_spec(name, path, target)
        if (
            spec is not None
            and spec.origin is not None
            and spec.origin.endswith(".py")
            and _is_rewrite_target(spec.origin)
        ):
            spec.loader = _RewriteLoader(spec.name, spec.origin)
            return spec
        # Let the default machinery handle everything else.
        return None


_FINDER = _RewriteFinder()


def install():
    if not any(isinstance(finder, _RewriteFinder) for finder in sys.meta_path):
        sys.meta_path.insert(0, _FINDER)
