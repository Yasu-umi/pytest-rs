"""caplog: log record capture fixture (Python-level replacement).

Mirrors pytest's LoggingPlugin: per-phase capture via two persistent
handlers — caplog_handler backs the caplog fixture API, report_handler
collects the "Captured log {when}" failure-report sections (kept separate
so caplog.at_level(DEBUG) does not leak DEBUG lines into reports). The
engine drives phases via start_phase()/finish_item().
"""

import contextlib
import datetime
import io
import logging
import os
import pickle
import sys
import types

from pytest._fixtures import fixture

#: pytest's DEFAULT_LOG_FORMAT.
DEFAULT_LOG_FORMAT = "%(levelname)-8s %(name)s:%(filename)s:%(lineno)s %(message)s"


class _StashKey:
    """Sentinel key into item.stash (pytest StashKey equivalent)."""


caplog_records_key = _StashKey()
caplog_handler_key = _StashKey()


class catching_logs:
    """Context manager that prepares the whole logging machinery properly."""

    __slots__ = ("handler", "level", "orig_level")

    def __init__(self, handler, level=None):
        self.handler = handler
        self.level = level

    def __enter__(self):
        root_logger = logging.getLogger()
        if self.level is not None:
            self.handler.setLevel(self.level)
        root_logger.addHandler(self.handler)
        if self.level is not None:
            self.orig_level = root_logger.level
            root_logger.setLevel(min(self.orig_level, self.level))
        return self.handler

    def __exit__(self, exc_type, exc_val, exc_tb):
        root_logger = logging.getLogger()
        if self.level is not None:
            root_logger.setLevel(self.orig_level)
        root_logger.removeHandler(self.handler)


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


class _LiveLogHandler(logging.StreamHandler):
    """log_cli live terminal handler: prints a centered "live log {when}"
    section header before the first record of each phase."""

    def __init__(self):
        super().__init__(sys.stdout)
        self.when = None
        self._header_printed = False

    def set_when(self, when):
        self.when = when
        self._header_printed = False

    def emit(self, record):
        from pytest import _capture

        # Like pytest's _LiveLoggingStreamHandler: emit with the global
        # capture suspended, so live records reach the real terminal even
        # under fd-level capture.
        with _capture.state.globally_disabled():
            if not self._header_printed and self.when is not None:
                self._header_printed = True
                title = f" live log {self.when} "
                self.stream.write(f"{title:-^80}\n")
            super().emit(record)
            self.flush()


class _RelayHandler(logging.Handler):
    """PYTEST_RS_LOG_RELAY: pickle every record captured during a phase
    into the named file; the parent pytester run replays them into its own
    logging system (upstream's in-process runpytest propagates the inner
    run's records to the parent's caplog live). Attached to root only
    alongside the phase handlers — a permanent root handler would turn the
    suite-under-test's logging.basicConfig() into a no-op and suppress
    logging.lastResort."""

    def __init__(self, path):
        super().__init__(logging.NOTSET)
        self._path = path

    def emit(self, record):
        # SocketHandler-style: merge args into msg and drop unpicklables.
        payload = dict(record.__dict__)
        payload["msg"] = record.getMessage()
        payload["args"] = None
        payload["exc_info"] = None
        payload.pop("message", None)
        try:
            data = pickle.dumps(payload)
        except Exception:
            return
        with open(self._path, "ab") as f:
            f.write(data)


class DatetimeFormatter(logging.Formatter):
    """%f (microseconds) support in datefmt, like upstream pytest."""

    def formatTime(self, record, datefmt=None):
        if datefmt and "%f" in datefmt:
            ct = datetime.datetime.fromtimestamp(record.created).astimezone()
            return ct.strftime(datefmt)
        return super().formatTime(record, datefmt)


class _NullCliHandler(logging.NullHandler):
    """Placeholder when live CLI logging is disabled (pytest parity:
    LoggingPlugin.log_cli_handler is always non-None)."""

    def reset(self):
        pass

    def set_when(self, when):
        pass


def _parse_level(value):
    """An int log level from an int-ish or name string, else None."""
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        pass
    named = logging.getLevelName(str(value).strip().upper())
    return named if isinstance(named, int) else None


class LoggingState:
    """Per-run logging plugin state (pytest's LoggingPlugin equivalent);
    config.pluginmanager.getplugin("logging-plugin") returns this."""

    def __init__(self):
        self.log_level = None  # int once log_level ini / --log-level is set
        formatter = logging.Formatter(DEFAULT_LOG_FORMAT)
        self.formatter = formatter
        self.caplog_handler = LogCaptureHandler()
        self.caplog_handler.setFormatter(formatter)
        self.report_handler = LogCaptureHandler()
        self.report_handler.setFormatter(formatter)
        self.stash = {caplog_records_key: {}}
        self.sections = []  # finished (when, text) pairs for the current item
        self.when = None
        self._root_level_restore = None
        self._subtest_parent_log = []
        # Session-wide handlers, wired by configure().
        self.log_cli_enabled = False
        self.log_cli_handler = None
        self.log_file_handler = None
        self.relay_handler = None
        self._report_formatter = None

    def configure(self, settings):
        """Wire session handlers from CLI/ini settings (a str->str dict with
        keys like log_cli, log_cli_level, log_file, log_disable...)."""

        def get(key):
            value = settings.get(key)
            return value if value not in (None, "") else None

        log_level = _parse_level(get("log_level"))
        log_format = get("log_format") or DEFAULT_LOG_FORMAT
        log_date_format = get("log_date_format")
        # Captured-section formatter follows log_format/log_date_format ini.
        self._report_formatter = DatetimeFormatter(log_format, datefmt=log_date_format)
        self.formatter = self._report_formatter
        self.caplog_handler.setFormatter(self._report_formatter)
        self.report_handler.setFormatter(self._report_formatter)

        root = logging.getLogger()
        explicit_levels = []

        # --- log_file -----------------------------------------------------
        log_file = get("log_file")
        if log_file:
            mode = get("log_file_mode") or "w"
            os.makedirs(os.path.dirname(os.path.abspath(log_file)), exist_ok=True)
            handler = logging.FileHandler(log_file, mode=mode, encoding="utf-8")
            file_level = _parse_level(get("log_file_level"))
            effective = file_level if file_level is not None else log_level
            handler.setLevel(effective if effective is not None else logging.NOTSET)
            fmt = get("log_file_format") or log_format
            datefmt = get("log_file_date_format") or log_date_format
            handler.setFormatter(DatetimeFormatter(fmt, datefmt=datefmt))
            self.log_file_handler = handler
            root.addHandler(handler)
            if effective is not None:
                explicit_levels.append(effective)

        # --- log_cli ------------------------------------------------------
        cli_level = _parse_level(get("log_cli_level"))
        log_cli = str(settings.get("log_cli", "")).strip().lower() in ("true", "1", "yes", "on")
        # The log_cli_level *ini* sets the level but does not enable live
        # logging by itself; the --log-cli-level CLI option does.
        self.log_cli_enabled = log_cli or get("log_cli_level_from_cli") is not None
        if self.log_cli_enabled:
            handler = _LiveLogHandler()
            effective = cli_level if cli_level is not None else log_level
            handler.setLevel(effective if effective is not None else logging.NOTSET)
            fmt = get("log_cli_format") or log_format
            datefmt = get("log_cli_date_format") or log_date_format
            handler.setFormatter(DatetimeFormatter(fmt, datefmt=datefmt))
            self.log_cli_handler = handler
            root.addHandler(handler)
            if effective is not None:
                explicit_levels.append(effective)
            sys.stdout.flush()
        else:
            self.log_cli_handler = _NullCliHandler()

        # An explicit level lowers the root logger so records reach the
        # session handlers; without one the root default (WARNING) stands.
        if explicit_levels:
            root.setLevel(min([root.level, *explicit_levels]))

        # --logger-disable / log_disable.
        for name in (get("log_disable") or "").split("\n"):
            name = name.strip()
            if name:
                logging.getLogger(name).disabled = True

        # A pytester parent asked for this run's records (see _RelayHandler).
        relay_path = os.environ.get("PYTEST_RS_LOG_RELAY")
        if relay_path:
            self.relay_handler = _RelayHandler(relay_path)

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
        if self.log_cli_handler is not None:
            self.log_cli_handler.set_when(when)
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
        if self.relay_handler is not None:
            root.addHandler(self.relay_handler)

    def end_phase(self):
        if self.when is None:
            return
        root = logging.getLogger()
        root.removeHandler(self.caplog_handler)
        root.removeHandler(self.report_handler)
        if self.relay_handler is not None:
            root.removeHandler(self.relay_handler)
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
        self._subtest_parent_log.clear()

    def subtest_enter(self):
        if self.when is None:
            return
        text = self.report_handler.stream.getvalue().strip()
        if text:
            self._subtest_parent_log.append(text)
        self.report_handler.reset()

    def subtest_exit(self):
        if self.when is None:
            return []
        text = self.report_handler.stream.getvalue().strip()
        self.report_handler.reset()
        if text:
            return [(f"Captured log {self.when}", text)]
        return []

    def failure_sections(self):
        """pytest-style (title, text) report sections for a failing report."""
        out = [(f"Captured log {when}", text) for when, text in self.sections]
        if self.when is not None:
            parent_log = "\n".join(self._subtest_parent_log)
            text = self.report_handler.stream.getvalue().strip()
            combined = "\n".join(filter(None, [parent_log, text]))
            if combined:
                out.append((f"Captured log {self.when}", combined))
        return out


state = LoggingState()


def configure(settings):
    state.configure(settings)


def set_live_when(when):
    """Relabel the live (log_cli) section header outside the normal phases
    (start/finish/collection)."""
    if state.log_cli_handler is not None:
        state.log_cli_handler.set_when(when)


def log_cli_enabled():
    return state.log_cli_enabled


def start_phase(when, level=None):
    state.start_phase(when, level)


def end_phase():
    # Closes a phase outside the item cycle (the collection catching_logs):
    # with a root handler attached, a module-level logging call during
    # import cannot trigger logging.basicConfig (issue #6240).
    state.end_phase()


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


import re as _re

_ANSI_ESCAPE_SEQ = _re.compile(r"\x1b\[[\d;]+m")


def _remove_ansi_escape_sequences(text):
    return _ANSI_ESCAPE_SEQ.sub("", text)


class DatetimeFormatter(logging.Formatter):
    def formatTime(self, record, datefmt=None):
        if datefmt and "%f" in datefmt:
            ct = self.converter(record.created)
            tz = datetime.timezone(
                datetime.timedelta(seconds=ct.tm_gmtoff), ct.tm_zone
            )
            dt = datetime.datetime(
                *ct[0:6], microsecond=int(record.msecs * 1000), tzinfo=tz
            )
            return dt.strftime(datefmt)
        return super().formatTime(record, datefmt)


class ColoredLevelFormatter(DatetimeFormatter):
    LOGLEVEL_COLOROPTS = {
        logging.CRITICAL: {"red"},
        logging.ERROR: {"red", "bold"},
        logging.WARNING: {"yellow"},
        logging.WARN: {"yellow"},
        logging.INFO: {"green"},
        logging.DEBUG: {"purple"},
        logging.NOTSET: set(),
    }
    LEVELNAME_FMT_REGEX = _re.compile(r"%\(levelname\)([+-.]?\d*(?:\.\d+)?s)")

    def __init__(self, terminalwriter, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._terminalwriter = terminalwriter
        self._original_fmt = self._style._fmt
        self._level_to_fmt_mapping = {}
        for level, color_opts in self.LOGLEVEL_COLOROPTS.items():
            self.add_color_level(level, *color_opts)

    def add_color_level(self, level, *color_opts):
        assert self._fmt is not None
        levelname_fmt_match = self.LEVELNAME_FMT_REGEX.search(self._fmt)
        if not levelname_fmt_match:
            return
        levelname_fmt = levelname_fmt_match.group()
        formatted_levelname = levelname_fmt % {"levelname": logging.getLevelName(level)}
        color_kwargs = {name: True for name in color_opts}
        colorized_formatted_levelname = self._terminalwriter.markup(
            formatted_levelname, **color_kwargs
        )
        self._level_to_fmt_mapping[level] = self.LEVELNAME_FMT_REGEX.sub(
            colorized_formatted_levelname, self._fmt
        )

    def format(self, record):
        fmt = self._level_to_fmt_mapping.get(record.levelno, self._original_fmt)
        self._style._fmt = fmt
        return super().format(record)


class PercentStyleMultiline(logging.PercentStyle):
    def __init__(self, fmt, auto_indent):
        super().__init__(fmt)
        self._auto_indent = self._get_auto_indent(auto_indent)

    @staticmethod
    def _get_auto_indent(auto_indent_option):
        if auto_indent_option is None:
            return 0
        elif isinstance(auto_indent_option, bool):
            return -1 if auto_indent_option else 0
        elif isinstance(auto_indent_option, int):
            return int(auto_indent_option)
        elif isinstance(auto_indent_option, str):
            try:
                return int(auto_indent_option)
            except ValueError:
                pass
            val = auto_indent_option.lower()
            if val in ("y", "yes", "t", "true", "on", "1"):
                return -1
            elif val in ("n", "no", "f", "false", "off", "0"):
                return 0
        return 0

    def format(self, record):
        if "\n" in record.message:
            if hasattr(record, "auto_indent"):
                auto_indent = self._get_auto_indent(record.auto_indent)
            else:
                auto_indent = self._auto_indent
            if auto_indent:
                lines = record.message.splitlines()
                formatted = self._fmt % {**record.__dict__, "message": lines[0]}
                if auto_indent < 0:
                    indentation = _remove_ansi_escape_sequences(formatted).find(
                        lines[0]
                    )
                else:
                    indentation = auto_indent
                lines[0] = formatted
                return ("\n" + " " * indentation).join(lines)
        return self._fmt % record.__dict__
