"""faulthandler builtin plugin: dump a traceback of all threads to stderr if
a test hangs past faulthandler_timeout (ports _pytest.faulthandler).

Not dispatched through the generic hookimpl scan -- bootstrap.rs's
is_internal_shim_module_name excludes every _pytest.*/pytest.* module from
it, so the engine calls configure()/unconfigure()/start_timeout()/
cancel_timeout() explicitly at the right lifecycle points instead (session
start/end, and around each item's whole setup/call/teardown protocol).

_stack holds one entry per active configure()/unconfigure() pair, LIFO like
a nested pytester run (each nested pytester.runpytest() reconfigures and
un-configures around its own inner session, restoring the outer state
exactly as upstream's per-Config stash does)."""

import os
import sys


def get_stderr_fileno():
    try:
        fileno = sys.stderr.fileno()
        # The Twisted Logger will return an invalid file descriptor since it
        # is not backed by an FD. So, let's also forward this to the same
        # code path as with pytest-xdist.
        if fileno == -1:
            raise AttributeError()
        return fileno
    except (AttributeError, ValueError):
        # pytest-xdist monkeypatches sys.stderr with an object that is not
        # an actual file.
        return sys.__stderr__.fileno()


_stack: list[dict] = []


def configure(timeout, exit_on_timeout):
    import faulthandler

    # At teardown we want to restore the original faulthandler fileno, but
    # faulthandler has no API to return the original fileno, so stash the
    # stderr fileno here to use later.
    stderr_fileno = get_stderr_fileno()
    original_fd = stderr_fileno if faulthandler.is_enabled() else None
    dup_fd = os.dup(stderr_fileno)
    faulthandler.enable(file=dup_fd)
    _stack.append(
        {
            "original_fd": original_fd,
            "dup_fd": dup_fd,
            "timeout": timeout,
            "exit_on_timeout": exit_on_timeout,
        }
    )


def unconfigure():
    import faulthandler

    entry = _stack.pop()
    faulthandler.disable()
    os.close(entry["dup_fd"])
    # Re-enable the faulthandler if it was originally enabled.
    if entry["original_fd"] is not None:
        faulthandler.enable(entry["original_fd"])


def start_timeout():
    """Arm the timeout for the item about to run its whole setup/call/
    teardown protocol (mirrors upstream's pytest_runtest_protocol
    hookwrapper, which spans all three phases, not just call)."""
    if not _stack:
        return
    entry = _stack[-1]
    if entry["timeout"] > 0:
        import faulthandler

        faulthandler.dump_traceback_later(
            entry["timeout"], file=entry["dup_fd"], exit=entry["exit_on_timeout"]
        )


def cancel_timeout():
    import faulthandler

    faulthandler.cancel_dump_traceback_later()


def pytest_enter_pdb():
    """Cancel any traceback dumping due to timeout before entering pdb."""
    cancel_timeout()


def pytest_exception_interact():
    """Cancel any traceback dumping due to an interactive exception being
    raised."""
    cancel_timeout()
