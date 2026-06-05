"""pytest-xdist API shim: worker-detection helpers backed by pytest-rs's
native -n process workers (execnet/DSession internals are not reproduced)."""

__version__ = "3.8.0"


def is_xdist_worker(request_or_session) -> bool:
    """True if this is an xdist worker (-n) process."""
    return hasattr(request_or_session.config, "workerinput")


def is_xdist_controller(request_or_session) -> bool:
    """True if this is the xdist controller of a distributed run."""
    return (
        not is_xdist_worker(request_or_session)
        and getattr(request_or_session.config.option, "dist", "no") != "no"
    )


# ``is_xdist_master`` is the deprecated alias kept by upstream.
is_xdist_master = is_xdist_controller


def get_xdist_worker_id(request_or_session) -> str:
    """The worker id ("gw0", ...), or "master" in the controller."""
    if hasattr(request_or_session.config, "workerinput"):
        return request_or_session.config.workerinput["workerid"]
    return "master"
