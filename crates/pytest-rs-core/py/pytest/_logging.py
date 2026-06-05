"""caplog: log record capture fixture (Python-level replacement)."""

import contextlib
import logging

from pytest._fixtures import fixture


class LogCaptureHandler(logging.StreamHandler):
    """A logging handler that stores records and the formatted text."""

    def __init__(self):
        import io

        super().__init__(io.StringIO())
        self.records = []

    def emit(self, record):
        self.records.append(record)
        super().emit(record)

    def reset(self):
        self.records = []
        self.stream.seek(0)
        self.stream.truncate()


class LogCaptureFixture:
    """The object yielded by the caplog fixture."""

    def __init__(self):
        self.handler = LogCaptureHandler()
        self.handler.setFormatter(logging.Formatter("%(levelname)-8s %(name)s:%(filename)s:%(lineno)s %(message)s"))
        # (logger, old level) pairs restored at teardown.
        self._initial_logger_levels = {}
        self._initial_handler_level = None

    def _start(self):
        root = logging.getLogger()
        root.addHandler(self.handler)

    def _stop(self):
        root = logging.getLogger()
        root.removeHandler(self.handler)
        for logger, level in self._initial_logger_levels.items():
            logging.getLogger(logger).setLevel(level)
        self._initial_logger_levels = {}
        if self._initial_handler_level is not None:
            self.handler.setLevel(self._initial_handler_level)
            self._initial_handler_level = None

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
        self.handler.reset()

    def get_records(self, when):
        # Phase-separated records are not tracked; "call" covers the test body.
        return self.records if when == "call" else []

    def set_level(self, level, logger=None):
        logger_obj = logging.getLogger(logger)
        self._initial_logger_levels.setdefault(logger or "", logger_obj.level)
        logger_obj.setLevel(level)
        if self._initial_handler_level is None:
            self._initial_handler_level = self.handler.level
        self.handler.setLevel(level)

    @contextlib.contextmanager
    def at_level(self, level, logger=None):
        logger_obj = logging.getLogger(logger)
        old_logger = logger_obj.level
        old_handler = self.handler.level
        logger_obj.setLevel(level)
        self.handler.setLevel(level)
        try:
            yield
        finally:
            logger_obj.setLevel(old_logger)
            self.handler.setLevel(old_handler)

    @contextlib.contextmanager
    def filtering(self, filter_):
        self.handler.addFilter(filter_)
        try:
            yield
        finally:
            self.handler.removeFilter(filter_)


@fixture
def caplog():
    capture = LogCaptureFixture()
    capture._start()
    yield capture
    capture._stop()
