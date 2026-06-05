"""--setup-show / --setup-only teardown-line printers."""


def teardown_printer(indent, scope_char, name):
    def printer():
        print(f"{indent}TEARDOWN {scope_char} {name}")

    return printer
