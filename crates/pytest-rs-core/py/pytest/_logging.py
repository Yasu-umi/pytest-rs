"""caplog: log record capture fixture (Python-level replacement).

Mirrors pytest's LoggingPlugin: per-phase capture via two persistent
handlers — caplog_handler backs the caplog fixture API, report_handler
collects the "Captured log {when}" failure-report sections (kept separate
so caplog.at_level(DEBUG) does not leak DEBUG lines into reports). The
engine drives phases via start_phase()/finish_item().
"""

import contextlib
import io
import logging
import types

from pytest._fixtures import fixture

#: pytest's DEFAULT_LOG_FORMAT.
DEFAULT_LOG_FORMAT = "%(levelname)-8s %(name)s:%(filename)s:%(lineno)s %(message)s"


class _StashKey:
    """Sentinel key into item.stash (pytest StashKey equivalent)."""


caplog_records_key = _StashKey()
caplog_handler_key = _StashKey()


class LogCaptureHandler(logging.StreamHandler):
    """A logging handler that stores records and the formatted text."""

    def __init__(self):
        super().__init__(io.StringIO())
        self.records = []

    def emit(self, record):
        self.records.append(record)
        super().emit(record)

    def reset(self):
        # Rebinds (pytest semantics): per-phase record lists already stored
        # in the stash stay alive for get_records().
        self.records = []
        self.stream = io.StringIO()

    def clear(self):
        # In place (caplog.clear()): the stash's current-phase list follows.
        self.records.clear()
        self.stream = io.StringIO()


class LoggingState:
    """Per-run logging plugin state (pytest's LoggingPlugin equivalent);
    config.pluginmanager.getplugin("logging-plugin") returns this."""

    def __init__(self):
        self.log_level = None  # int once log_level ini / --log-level is set
        formatter = logging.Formatter(DEFAULT_LOG_FORMAT)
        self.caplog_handler = LogCaptureHandler()
        self.caplog_handler.setFormatter(formatter)
        self.report_handler = LogCaptureHandler()
        self.report_handler.setFormatter(formatter)
        self.stash = {caplog_records_key: {}}
        self.sections = []  # finished (when, text) pairs for the current item
        self.when = None
        self._root_level_restore = None

    def _set_level_config(self, level):
        if level is None:
            self.log_level = None
            return
        try:
            self.log_level = int(level)
            return
        except (TypeError, ValueError):
            pass
        named = logging.getLevelName(str(level).upper())
        self.log_level = named if isinstance(named, int) else None

    def start_phase(self, when, level=None):
        if self.when is not None:
            self.end_phase()
        if when == "setup":
            self.stash = {caplog_records_key: {}}
            self.sections = []
        self._set_level_config(level)
        self.when = when
        for handler in (self.caplog_handler, self.report_handler):
            handler.reset()
            if self.log_level is not None:
                handler.setLevel(self.log_level)
        self.stash[caplog_records_key][when] = self.caplog_handler.records
        self.stash[caplog_handler_key] = self.caplog_handler
        root = logging.getLogger()
        if self.log_level is not None:
            self._root_level_restore = root.level
            root.setLevel(min(root.level, self.log_level))
        root.addHandler(self.caplog_handler)
        root.addHandler(self.report_handler)

    def end_phase(self):
        if self.when is None:
            return
        root = logging.getLogger()
        root.removeHandler(self.caplog_handler)
        root.removeHandler(self.report_handler)
        if self._root_level_restore is not None:
            root.setLevel(self._root_level_restore)
            self._root_level_restore = None
        text = self.report_handler.stream.getvalue().strip()
        if text:
            self.sections.append((self.when, text))
        self.when = None

    def finish_item(self):
        self.end_phase()
        self.sections = []

    def failure_sections(self):
        """pytest-style (title, text) report sections for a failing report."""
        out = [(f"Captured log {when}", text) for when, text in self.sections]
        if self.when is not None:
            text = self.report_handler.stream.getvalue().strip()
            if text:
                out.append((f"Captured log {self.when}", text))
        return out


state = LoggingState()


def start_phase(when, level=None):
    state.start_phase(when, level)


def finish_item():
    state.finish_item()


def failure_sections():
    return state.failure_sections()


class LogCaptureFixture:
    """The object yielded by the caplog fixture."""

    def __init__(self, item):
        self._item = item
        # Levels (and the logging.disable level) restored at teardown.
        self._initial_logger_levels = {}
        self._initial_handler_level = None
        self._initial_disabled_logging_level = None

    def _finalize(self):
        if self._initial_handler_level is not None:
            self.handler.setLevel(self._initial_handler_level)
            self._initial_handler_level = None
        for logger, level in self._initial_logger_levels.items():
            logging.getLogger(logger).setLevel(level)
        self._initial_logger_levels = {}
        if self._initial_disabled_logging_level is not None:
            logging.disable(self._initial_disabled_logging_level)
            self._initial_disabled_logging_level = None

    @property
    def handler(self):
        return self._item.stash[caplog_handler_key]

    def get_records(self, when):
        return self._item.stash[caplog_records_key].get(when, [])

    @property
    def records(self):
        return self.handler.records

    @property
    def text(self):
        return self.handler.stream.getvalue()

    @property
    def messages(self):
        return [record.getMessage() for record in self.records]

    @property
    def record_tuples(self):
        return [(r.name, r.levelno, r.getMessage()) for r in self.records]

    def clear(self):
        self.handler.clear()

    def _force_enable_logging(self, level, logger_obj):
        """Un-disable (logging.disable) levels >= the requested capture level.

        Returns the original disabled level so callers can restore it.
        """
        original_disable_level = logger_obj.manager.disable

        if isinstance(level, str):
            # Try to translate the level string to an int for logging.disable().
            level = logging.getLevelName(level)

        if not isinstance(level, int):
            # The level provided was not valid, so just un-disable all logging.
            logging.disable(logging.NOTSET)
        elif not logger_obj.isEnabledFor(level):
            # Each level is 10 away from other levels.
            logging.disable(max(level - 10, logging.NOTSET))

        return original_disable_level

    def set_level(self, level, logger=None):
        logger_obj = logging.getLogger(logger)
        self._initial_logger_levels.setdefault(logger or "", logger_obj.level)
        logger_obj.setLevel(level)
        if self._initial_handler_level is None:
            self._initial_handler_level = self.handler.level
        self.handler.setLevel(level)
        initial_disabled_logging_level = self._force_enable_logging(level, logger_obj)
        if self._initial_disabled_logging_level is None:
            self._initial_disabled_logging_level = initial_disabled_logging_level

    @contextlib.contextmanager
    def at_level(self, level, logger=None):
        logger_obj = logging.getLogger(logger)
        old_logger = logger_obj.level
        old_handler = self.handler.level
        logger_obj.setLevel(level)
        self.handler.setLevel(level)
        original_disable_level = self._force_enable_logging(level, logger_obj)
        try:
            yield
        finally:
            logger_obj.setLevel(old_logger)
            self.handler.setLevel(old_handler)
            logging.disable(original_disable_level)

    @contextlib.contextmanager
    def filtering(self, filter_):
        self.handler.addFilter(filter_)
        try:
            yield
        finally:
            self.handler.removeFilter(filter_)


@fixture
def caplog():
    # The item proxy snapshots this item's stash (rebound per item by
    # start_phase("setup"), which always precedes fixture setup).
    capture = LogCaptureFixture(types.SimpleNamespace(stash=state.stash))
    yield capture
    capture._finalize()
