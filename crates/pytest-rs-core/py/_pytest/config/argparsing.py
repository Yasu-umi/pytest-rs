import argparse

import _pytest._io
from _pytest._stub import __getattr__  # noqa: F401


class DropShorterLongHelpFormatter(argparse.HelpFormatter):
    """Shorten help for long options that differ only in extra hyphens.

    - Collapse **long** options that are the same except for extra hyphens.
    - Shortcut if there are only two options and one of them is a short one.
    - Cache result on the action object as this is called at least 2 times.
    """

    def __init__(self, *args, **kwargs):
        # Use more accurate terminal width.
        if "width" not in kwargs:
            kwargs["width"] = _pytest._io.get_terminal_width()
        super().__init__(*args, **kwargs)

    def _format_action_invocation(self, action):
        orgstr = super()._format_action_invocation(action)
        if orgstr and orgstr[0] != "-":  # only optional arguments
            return orgstr
        res = getattr(action, "_formatted_action_invocation", None)
        if res:
            return res
        options = orgstr.split(", ")
        if len(options) == 2 and (len(options[0]) == 2 or len(options[1]) == 2):
            # a shortcut for '-h, --help' or '--abc', '-a'
            action._formatted_action_invocation = orgstr
            return orgstr
        return_list = []
        short_long = {}
        for option in options:
            if len(option) == 2 or option[2] == " ":
                continue
            if not option.startswith("--"):
                raise argparse.ArgumentError(
                    action, f'long optional argument without "--": [{option}]'
                )
            xxoption = option[2:]
            shortened = xxoption.replace("-", "")
            if shortened not in short_long or len(short_long[shortened]) < len(xxoption):
                short_long[shortened] = xxoption
        # now short_long has been filled out to the longest with dashes
        # **and** we keep the right option ordering from add_argument
        for option in options:
            if len(option) == 2 or option[2] == " ":
                return_list.append(option)
            if option[2:] == short_long.get(option.replace("-", "")):
                return_list.append(option.replace(" ", "=", 1))
        formatted_action_invocation = ", ".join(return_list)
        action._formatted_action_invocation = formatted_action_invocation
        return formatted_action_invocation

    def _split_lines(self, text, width):
        """Wrap lines after splitting on original newlines.

        This allows to have explicit line breaks in the help text.
        """
        import textwrap

        lines = []
        for line in text.splitlines():
            lines.extend(textwrap.wrap(line.strip(), width))
        return lines


def render_option_group(name, options):
    """Render one `--help` option group (upstream's `parser.getgroup(name)`
    section, e.g. pytest-benchmark's `benchmark:` heading) via the real
    `argparse` engine + `DropShorterLongHelpFormatter`, so the `--opt=VALUE`
    collapsing, line-wrapping, and same-line-vs-next-line help placement
    match upstream exactly (both are upstream's own machinery).

    `options` is a list of dicts, each either `{"flags": [...], "help": ...,
    "flag": True}` (a store_true switch) or `{"flags": [...], "help": ...,
    "metavar": ..., "nargs": "?"|"+"|None}` (a value-taking option).
    """
    parser = argparse.ArgumentParser(add_help=False, formatter_class=DropShorterLongHelpFormatter)
    group = parser.add_argument_group(name)
    for opt in options:
        kwargs = {"help": opt["help"]}
        if opt.get("flag"):
            kwargs["action"] = "store_true"
        else:
            kwargs["metavar"] = opt["metavar"]
            if opt.get("nargs"):
                kwargs["nargs"] = opt["nargs"]
        group.add_argument(*opt["flags"], **kwargs)
    text = parser.format_help()
    marker = f"{name}:\n"
    return text[text.index(marker) :]


def get_ini_default_for_type(type):
    """Used by addini to get the default value for a given config option
    type, when default is not supplied."""
    if type in ("paths", "pathlist", "args", "linelist"):
        return []
    elif type == "bool":
        return False
    elif type == "int":
        return 0
    elif type == "float":
        return 0.0
    else:
        return ""
