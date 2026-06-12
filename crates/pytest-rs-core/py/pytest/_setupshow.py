"""--setup-show / --setup-only teardown-line printers."""

import sys


def teardown_printer(indent, scope_char, name):
    def printer():
        from pytest import _capture

        # The narration must reach the real terminal, not the capture;
        # pytest's tw.line() style: a leading newline closes the current
        # line (e.g. a pending progress char), no trailing one.
        with _capture.state.globally_disabled():
            print(f"\n{indent}TEARDOWN {scope_char} {name}", end="")
            sys.stdout.flush()

    return printer
