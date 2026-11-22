"""Record --affected dependencies inside a Python subprocess (stdlib only).

Started by the pytest-rs-affected .pth line when the parent session exported
PYTEST_RS_AFFECTED_OUT and PYTEST_RS_AFFECTED_CHILD. It records the project
code this process runs (file and qualified name of each code object), the
files it opens for reading and the directories it lists under the rootdir,
and how many subprocesses it starts itself. At exit it writes one JSON file
into PYTEST_RS_AFFECTED_OUT, tagged with the parent's context
(PYTEST_RS_AFFECTED_CONTEXT, the context current when this process was
started), for the parent to fold into that context.

Environment (all set by the parent session):
    PYTEST_RS_AFFECTED_OUT       directory for the record files
    PYTEST_RS_AFFECTED_CONTEXT   the parent's context id
    PYTEST_RS_AFFECTED_ROOT      rootdir, with a trailing separator
    PYTEST_RS_AFFECTED_EXCLUDED  os.pathsep-joined directories never recorded
    PYTEST_RS_AFFECTED_IGNORED   os.pathsep-joined files/dirs never recorded as inputs
"""

import atexit
import json
import os
import sys

_OUT = os.environ.get("PYTEST_RS_AFFECTED_OUT", "")
_CONTEXT = os.environ.get("PYTEST_RS_AFFECTED_CONTEXT", "")
_ROOT = os.environ.get("PYTEST_RS_AFFECTED_ROOT", "")
_EXCLUDED = [d for d in os.environ.get("PYTEST_RS_AFFECTED_EXCLUDED", "").split(os.pathsep) if d]
_IGNORED = [d for d in os.environ.get("PYTEST_RS_AFFECTED_IGNORED", "").split(os.pathsep) if d]
_SKIP_SUFFIXES = (".py", ".pyc", ".pyo", ".so", ".pyd")
_PYTHON_NAMES = ("python", "python3")


def _start():
    if not (_OUT and _CONTEXT and _ROOT):
        return
    monitoring = getattr(sys, "monitoring", None)
    if monitoring is None:
        return
    tool = None
    for candidate in (4, 3, 5):
        try:
            monitoring.use_tool_id(candidate, "pytest-rs-affected-child")
        except ValueError:
            continue
        tool = candidate
        break
    if tool is None:
        return

    codes = set()
    inputs = set()
    state = {"spawned": 0, "incomplete": False}

    def tracked(filename):
        return (
            filename.startswith(_ROOT)
            and filename.endswith(".py")
            and not any(filename.startswith(d) for d in _EXCLUDED)
            and "site-packages" not in filename.split(os.sep)
        )

    def py_start(code, offset):
        if tracked(code.co_filename):
            codes.add((code.co_filename, code.co_qualname))
        return monitoring.DISABLE

    monitoring.register_callback(tool, monitoring.events.PY_START, py_start)
    monitoring.set_events(tool, monitoring.events.PY_START)

    def input_path(path):
        try:
            path = os.fsdecode(path)
        except TypeError:
            return None
        path = os.path.abspath(path)
        if (
            not path.startswith(_ROOT)
            or any(path.startswith(d) for d in _EXCLUDED)
            or any(path == i or path.startswith(i + os.sep) for i in _IGNORED)
            or path.endswith(_SKIP_SUFFIXES)
            or "__pycache__" in path.split(os.sep)
        ):
            return None
        return path

    def raised_by_import_system():
        # The caller of the audited function: two frames up from here
        # (audit -> this), when the stack is that deep.
        frame = sys._getframe(1)
        for _ in range(2):
            if frame is None:
                return False
            frame = frame.f_back
        if frame is None:
            return False
        filename = frame.f_code.co_filename
        return filename.startswith("<frozen importlib") or "importlib/_bootstrap" in filename

    def raised_by_subprocess():
        # subprocess.Popen may start the child through os.posix_spawn
        # (Linux): the same spawn, already counted by subprocess.Popen.
        frame = sys._getframe(1)
        for _ in range(2):
            if frame is None:
                return False
            frame = frame.f_back
        return frame is not None and os.path.basename(frame.f_code.co_filename) == "subprocess.py"

    def is_python(executable, args):
        program = executable or (args[0] if isinstance(args, (list, tuple)) and args else args)
        try:
            name = os.path.basename(os.fsdecode(program))
        except TypeError:
            return False
        return name.startswith(_PYTHON_NAMES) or os.fsdecode(program) == sys.executable

    def audit(event, args):
        # An exception raised here would abort the audited operation (the
        # open, the spawn) in the program being recorded: never let one out.
        try:
            record_event(event, args)
        except Exception:
            state["incomplete"] = True

    def record_event(event, args):
        if event == "open":
            mode = args[1] if len(args) > 1 else None
            flags = args[2] if len(args) > 2 else 0
            if isinstance(mode, str):
                if any(c in mode for c in "wax+"):
                    return
            elif isinstance(flags, int) and flags & 0b11:
                return
            if isinstance(args[0], int) or raised_by_import_system():
                return
            path = input_path(args[0])
            if path is not None and not os.path.isdir(path):
                inputs.add((path, "content"))
        elif event in ("os.listdir", "os.scandir"):
            if raised_by_import_system():
                return
            path = input_path(args[0] if args and args[0] is not None else ".")
            if path is not None:
                inputs.add((path, "listing"))
        elif event == "subprocess.Popen":
            executable, popen_args = args[0], args[1]
            state["spawned"] += 1
            # A shell command line can run anything: not countable.
            if isinstance(popen_args, (str, bytes)) or not is_python(executable, popen_args):
                state["incomplete"] = True
        elif event == "os.posix_spawn":
            if raised_by_subprocess():
                return
            state["spawned"] += 1
            if not is_python(args[0], None):
                state["incomplete"] = True
        elif event in ("os.system", "os.exec", "os.spawn", "os.fork", "os.forkpty", "os.startfile"):
            state["incomplete"] = True

    sys.addaudithook(audit)

    def dump():
        monitoring.set_events(tool, 0)
        record = {
            "context": int(_CONTEXT),
            "codes": sorted(codes),
            "inputs": sorted(inputs),
            "spawned": state["spawned"],
            "incomplete": state["incomplete"],
        }
        path = os.path.join(_OUT, f"{_CONTEXT}-{os.getpid()}.json")
        tmp = path + ".tmp"
        try:
            with open(tmp, "w") as f:
                json.dump(record, f)
            os.replace(tmp, path)
        except OSError:
            pass

    atexit.register(dump)


_start()
