"""Submit failure or test session information to a pastebin service."""

from __future__ import annotations

import re
import urllib.error
import urllib.parse
import urllib.request


def create_new_paste(contents: str | bytes) -> str:
    """Create a new paste using the bpaste.net service.

    :contents: Paste contents string.
    :returns: URL to the pasted contents, or an error message.
    """
    url = "https://bpa.st"
    if isinstance(contents, str):
        contents = contents.encode("utf-8")
    params = {"code": contents, "lexer": "text", "expiry": "1week"}
    try:
        response = (
            urllib.request.urlopen(url, data=urllib.parse.urlencode(params).encode("ascii"))
            .read()
            .decode("utf-8")
        )
    except urllib.error.HTTPError as e:
        with e:
            return f"bad response: {e}"
    except OSError as e:
        return f"bad response: {e}"
    m = re.search(r'href="/raw/(\w+)"', response)
    if m:
        return f"{url}/show/{m.group(1)}"
    else:
        return "bad response: invalid format ('" + response + "')"


from _pytest._stub import __getattr__  # noqa: E402, F401
