"""Interactive debugging with PDB, the Python Debugger.

Lightweight reimplementation of _pytest.debugging for pytest-rs.  The
Rust engine parses --pdb / --pdbcls / --trace and sets config.option
attributes; this module reads them, imports the requested debugger class,
and provides the PdbInvoke plugin whose pytest_exception_interact hook
drops into the debugger on failures.
"""

from __future__ import annotations

import functools
import os
import sys
from typing import Any


def _raw_write(text: str) -> None:
    """Write directly to fd 1, bypassing sys.stdout so in-process runs
    (where the outer test's capture replaces sys.stdout) see the output
    in the inner run's result."""
    data = text.encode("utf-8", errors="replace")
    try:
        os.write(1, data)
    except OSError:
        sys.stdout.write(text)


class pytestPDB:
    _config: Any = None
    _saved: list[tuple] = []
    _recursive_debug = 0
    _wrapped_pdb_cls: tuple | None = None

    @classmethod
    def _import_pdb_cls(cls, capman=None):
        if not cls._config:
            import pdb

            return pdb.Pdb

        usepdb_cls = getattr(cls._config.option, "usepdb_cls", None)

        if cls._wrapped_pdb_cls and cls._wrapped_pdb_cls[0] == usepdb_cls:
            return cls._wrapped_pdb_cls[1]

        if usepdb_cls:
            modname, classname = usepdb_cls
            try:
                __import__(modname)
                mod = sys.modules[modname]
                parts = classname.split(".")
                pdb_cls = getattr(mod, parts[0])
                for part in parts[1:]:
                    pdb_cls = getattr(pdb_cls, part)
            except Exception as exc:
                value = ":".join((modname, classname))
                raise RuntimeError(f"--pdbcls: could not import {value!r}: {exc}") from exc
        else:
            import pdb

            pdb_cls = pdb.Pdb

        wrapped_cls = cls._get_pdb_wrapper_class(pdb_cls, capman)
        cls._wrapped_pdb_cls = (usepdb_cls, wrapped_cls)
        return wrapped_cls

    @classmethod
    def _get_pdb_wrapper_class(cls, pdb_cls, capman):
        from pytest._outcomes import exit as pytest_exit

        class PytestPdbWrapper(pdb_cls):
            _pytest_capman = capman
            _continued = False

            def do_continue(self, arg):
                ret = super().do_continue(arg)
                if cls._recursive_debug == 0:
                    capturing = capman is not None
                    if capturing:
                        _raw_write("\n")
                        _raw_write(
                            ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> "
                            "PDB continue (IO-capturing resumed) "
                            ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n"
                        )
                    else:
                        _raw_write("\n")
                        _raw_write(
                            ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> "
                            "PDB continue "
                            ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n"
                        )
                self._continued = True
                return ret

            do_c = do_cont = do_continue

            def do_quit(self, arg):
                ret = super().do_quit(arg)
                if cls._recursive_debug == 0:
                    pytest_exit("Quitting debugger")
                return ret

            do_q = do_quit
            do_exit = do_quit

        return PytestPdbWrapper

    @classmethod
    def _init_pdb(cls, method, *args, **kwargs):
        from pytest._capture import manager as capman

        capman.suspend_global_capture(in_=True)

        _raw_write("\n")
        capturing = True
        try:
            from pytest._capture import state

            capturing = state._installed and state._capture is not None
        except Exception:
            pass

        if capturing:
            header = kwargs.pop("header", None)
            if header is not None:
                _raw_write(
                    f">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> {header} "
                    ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n"
                )
            else:
                _raw_write(
                    f">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> "
                    f"PDB {method} (IO-capturing turned off) "
                    ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n"
                )
        else:
            _raw_write(
                f">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> "
                f"PDB {method} "
                ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n"
            )

        _pdb = cls._import_pdb_cls(capman)(**kwargs)
        return _pdb

    @classmethod
    def set_trace(cls, *args, **kwargs) -> None:
        frame = sys._getframe().f_back
        _pdb = cls._init_pdb("set_trace", *args, **kwargs)
        _pdb.set_trace(frame)


class PdbInvoke:
    def pytest_exception_interact(self, node, call, report) -> None:
        import unittest

        from pytest._capture import manager as capman

        capman.suspend_global_capture(in_=True)
        out, err = capman.read_global_capture()
        _raw_write(out)
        _raw_write(err)

        excinfo = getattr(call, "excinfo", None)
        if excinfo is None:
            return

        exc_val = getattr(excinfo, "value", excinfo)
        if isinstance(exc_val, unittest.SkipTest):
            return

        _enter_pdb(node, excinfo, report)


class PdbTrace:
    def pytest_pyfunc_call(self, pyfuncitem) -> None:
        wrap_pytest_function_for_tracing(pyfuncitem)


def wrap_pytest_function_for_tracing(pyfuncitem) -> None:
    _pdb = pytestPDB._init_pdb("runcall")
    testfunction = pyfuncitem.obj

    @functools.wraps(testfunction)
    def wrapper(*args, **kwargs) -> None:
        func = functools.partial(testfunction, *args, **kwargs)
        _pdb.runcall(func)

    pyfuncitem.obj = wrapper


def _enter_pdb(node, excinfo, rep):
    showcapture = "all"
    try:
        showcapture = node.config.option.showcapture
    except Exception:
        pass

    _raw_write("\n")

    for sectionname, attr in [
        ("stdout", "capstdout"),
        ("stderr", "capstderr"),
        ("log", "caplog"),
    ]:
        content = getattr(rep, attr, "")
        if showcapture in (sectionname, "all") and content:
            _raw_write(
                f">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> "
                f"captured {sectionname} "
                ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n"
            )
            if content[-1:] == "\n":
                content = content[:-1]
            _raw_write(content + "\n")

    _raw_write(">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> traceback >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n")
    longrepr = getattr(rep, "longrepr", None)
    if longrepr:
        if hasattr(longrepr, "toterminal"):
            import io

            buf = io.StringIO()

            class _FakeWriter:
                def line(self, s="", **kw):
                    buf.write(s + "\n")

                def write(self, s, **kw):
                    buf.write(s)

                def sep(self, ch, title="", **kw):
                    buf.write(f"{ch * 5} {title} {ch * 5}\n")

                markup = False
                fullwidth = 80

            longrepr.toterminal(_FakeWriter())
            _raw_write(buf.getvalue())
        else:
            _raw_write(str(longrepr) + "\n")

    _raw_write(">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> entering PDB >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n")

    tb_or_exc = _postmortem_traceback(excinfo)
    rep._pdbshown = True
    post_mortem(tb_or_exc)


def _postmortem_traceback(excinfo):
    exc_val = getattr(excinfo, "value", excinfo)
    return exc_val


def post_mortem(tb_or_exc):
    from pytest._outcomes import exit as pytest_exit

    p = pytestPDB._init_pdb("post_mortem")
    p.reset()
    p.interaction(None, tb_or_exc)
    if p.quitting:
        pytest_exit("Quitting debugger")


def maybe_interact(item_proxy, exc, longrepr="") -> None:
    """Called by the Rust runner after a call-phase failure if --pdb is active.
    Fires pytest_exception_interact on the item's ihook."""
    if pytestPDB._config is None:
        return
    if not getattr(pytestPDB._config.option, "usepdb", False):
        return

    import bdb

    from pytest._outcomes import Skipped

    if isinstance(exc, (Skipped, bdb.BdbQuit)):
        return

    class _ExcInfo:
        def __init__(self, e):
            self.value = e
            self.type = type(e)
            self._excinfo = (type(e), e, e.__traceback__)

    class _Call:
        def __init__(self):
            self.when = "call"
            self.excinfo = _ExcInfo(exc)

    class _Report:
        def __init__(self):
            self.when = "call"
            self.nodeid = getattr(item_proxy, "nodeid", "")
            self.outcome = "failed"
            self.failed = True
            self.longrepr = longrepr
            self.capstdout = ""
            self.capstderr = ""
            self.caplog = ""

    call = _Call()
    report = _Report()

    from pytest._pluginmanager import pluginmanager as pm

    try:
        pm.hook.pytest_exception_interact(node=item_proxy, call=call, report=report)
    except Exception:
        pass


def configure(config) -> None:
    import pdb

    from pytest._pluginmanager import pluginmanager as pm

    usepdb = getattr(config.option, "usepdb", False)
    trace = getattr(config.option, "trace", False)

    if trace and not pm.get_plugin("pdbtrace"):
        pm.register(PdbTrace(), "pdbtrace")

    if usepdb and not pm.get_plugin("pdbinvoke"):
        pm.register(PdbInvoke(), "pdbinvoke")

    pytestPDB._saved.append((pdb.set_trace, pytestPDB._config))
    pdb.set_trace = pytestPDB.set_trace
    pytestPDB._config = config
