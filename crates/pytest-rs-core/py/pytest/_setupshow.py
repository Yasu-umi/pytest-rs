"""--setup-show / --setup-only teardown-line printers."""

import os


def teardown_printer(indent, scope_char, name):
    def printer():
        from pytest import _capture

        # The narration must reach the real terminal, not the capture;
        # pytest's tw.line() style: a leading newline closes the current
        # line (e.g. a pending progress char), no trailing one.
        # Use os.write(1, ...) instead of print() to write directly to fd 1:
        # globally_disabled() suspends the capture by restoring fd 1 to the
        # pre-capture fd (the real terminal, or the in-process pytester's
        # fd-redirect temp file), but it also restores sys.stdout to the
        # pre-capture object (which in a nested in-process run is the outer
        # session's capture CaptureIO). Writing via sys.stdout would therefore
        # route TEARDOWN lines into the outer capture rather than the inner
        # run's output — matching SETUP, which the Rust runner writes via
        # print!() / fd 1 directly.
        with _capture.state.globally_disabled():
            os.write(1, f"\n{indent}TEARDOWN {scope_char} {name}".encode())

    return printer
