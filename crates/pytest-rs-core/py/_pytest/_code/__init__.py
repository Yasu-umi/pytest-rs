class Source:
    def __init__(self, obj=None):
        import inspect
        import textwrap

        if obj is None:
            self.lines = []
        elif isinstance(obj, str):
            self.lines = textwrap.dedent(obj).strip().splitlines()
        else:
            self.lines = textwrap.dedent(inspect.getsource(obj)).strip().splitlines()

    def __str__(self):
        return "\n".join(self.lines)
