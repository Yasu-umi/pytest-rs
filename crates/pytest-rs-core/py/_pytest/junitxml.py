from _pytest._stub import __getattr__  # noqa: E402, F401

_ILLEGAL = [
    (0x00, 0x08),
    (0x0B, 0x0C),
    (0x0E, 0x1F),
    (0x7F, 0x84),
    (0x86, 0x9F),
    (0xD800, 0xDFFF),
    (0xFDD0, 0xFDEF),
    (0xFFFE, 0xFFFF),
]


def bin_xml_escape(arg):
    """Escape characters that are illegal in XML 1.0 as #x?? sequences."""

    def repl(ch):
        code = ord(ch)
        if code <= 0xFF:
            return f"#x{code:02X}"
        return f"#x{code:04X}"

    out = []
    for ch in str(arg):
        code = ord(ch)
        if any(low <= code <= high for low, high in _ILLEGAL):
            out.append(repl(ch))
        else:
            out.append(ch)
    return "".join(out)
