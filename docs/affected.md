# `--affected`: run only the tests affected by your changes

Experimental.

`--affected` records which code each test ran. On later runs it uses that record to run only the tests whose code, or the inputs they read, changed since then. Without an `--affected*` option pytest-rs behaves exactly as usual: nothing is recorded, nothing is monitored, nothing is written.

## Options

| Invocation | Selects | Records |
|---|---|---|
| `pytest-rs --affected` | yes (everything when there is no record yet) | the tests that ran |
| `pytest-rs --affected=record` | no: runs whatever the other arguments select | the tests that ran |
| `pytest-rs --affected=select` | yes | nothing |
| `pytest-rs --affected-merge A B ... --affected-data OUT` | merges recorded data files into `OUT` and runs no tests | — |

- The usual local loop is `--affected`. In CI, record on the main branch and select on pull requests (see [In CI](#in-ci)).
- The mode only binds as `--affected=MODE`, so `pytest-rs --affected tests/` still treats `tests/` as a path.
- The record lives in `.pytest-rs-affected` in the rootdir. `--affected-data=PATH` puts it elsewhere.
- When everything is deselected, the run exits 0 rather than 5.

A record is usable only when it was written by the same pytest-rs version, Python (version, implementation, platform) and set of installed packages (name and version of every distribution). Otherwise, or when there is none yet, everything runs, and the header line says why, for example `affected: select, .pytest-rs-affected (installed packages changed)`. A recording run then starts a fresh record.

## What is recorded

### Code units

Every Python file under the rootdir splits into units, and a test depends on units, not on whole files:

- **function**: one per `def`/`async def`, keyed by its qualified name. It covers the decorators, the signature and the body. A nested `def` or `class` is a unit of its own, and only its name counts in the enclosing function.
- **class**: the class header and its body statements. Methods and nested classes are units of their own, but their names count in the class, so adding, removing or renaming a method changes the class.
- **module**: the top-level statements other than `def` and `class`.

A unit's checksum ignores formatting and comments. Docstrings count. Units are keyed by name, not position, so inserting or reordering definitions changes nothing, and adding a function to a module changes no existing unit. Two definitions of the same name (`if X: def f(): ... else: def f(): ...`) form one unit.

Calling a function also makes its enclosing functions and classes, and its module, dependencies of the test. Code whose qualified name matches no unit of its file (generated at run time, for example) makes the whole file a dependency.

### When code runs

Each piece of code is recorded under the context in which it runs:

| Context | Covers | Is a dependency of |
|---|---|---|
| the test | its setup (function-scoped fixtures included), call and teardown | that test |
| a setup that outlives one test | a fixture with a scope above function, or an xunit `setup_module`/`setup_class`, while it is set up and while it is torn down | every test that uses it |
| a test module's import | importing and collecting one test file, including the project modules it imports for the first time | every test in that file |
| global | everything else: plugin and conftest imports, hooks, configuration, session start and end | every test |

Each project module's own execution (its module-level code, class bodies, and whatever they call) is also recorded on its own. A module runs once, inside whichever context imports it first, and its code counts toward that context as above. Its own record serves every other test file that reaches it through imports:

- A test file depends on the import-time code of every project module it imports, whether the import runs the module or finds it already imported (a cache hit). It also depends, transitively, on the modules those modules import. The imports are taken from the modules' top-level `import` and `from ... import` statements (including those under `if TYPE_CHECKING:`), and from the imports that actually ran while each module executed.
- A module first imported inside a test belongs to that test. A later test that calls anything in it depends on it through the function it calls.

Code that runs on another thread is attributed to the test that is running at the time. Over-attributing like this is safe: it only makes more tests run.

### Files, directories and subprocesses

While recording, an audit hook also notes:

- **files opened for reading** under the rootdir, by their content;
- **directories listed** with `os.listdir`/`os.scandir` (also through `os.walk`, `glob`, `pathlib`, `pkgutil.iter_modules`), by their entry names;
- **subprocesses started**:
  - A Python subprocess started with an argument list (`subprocess.run([sys.executable, "-m", "app"])`, or any program named `python*`) records its own dependencies: the project code it runs, the files it reads and the directories it lists. They count toward the test (or setup, or import) that started it. Its own Python subprocesses do the same. A `.pth` file in the environment's site-packages starts the recorder; it does nothing unless an `--affected` run has set it up.
  - Anything else cannot be seen into, so the dependencies of whatever started it are marked incomplete. That covers other programs, shell command lines (`shell=True`, `os.system`), `os.exec*`, `os.fork` without exec, and Python children that end without writing their record. Children end that way when they call `os._exit`, are killed, are still running when the session ends, or run another Python environment that lacks the `.pth` file.

Reads and listings by Python's import system and by pytest-rs itself are not recorded, nor are `.py`, `.pyc` and extension files, `__pycache__`, the data file and the cache directory.

### Per test

For each test the record keeps its dependencies (each with the checksum or hash it had when recorded), whether it failed, its duration, whether its dependencies are incomplete, and when it was recorded. Since every test carries its own checksums, entries recorded at different times and on different commits stay valid side by side.

## What runs

A test runs when any of these is true:

- a unit it depends on changed, disappeared, or its file no longer parses or no longer exists;
- a file it read or a directory it listed changed (a file that disappeared counts as changed);
- it is new (not in the record);
- it failed last time (it keeps running until it passes);
- its dependencies are incomplete (it, or a context it depends on, started a subprocess that cannot be seen into), unless `incomplete = "trust"`;
- it was named on the command line (`file.py::test_name`), it matches `always_run`, or a changed file matched a `file_triggers` entry that lists it;
- something changed that means a full run (below).

A subprocess that cannot be seen into, started outside any test, makes every test incomplete, so everything runs. This happens when a conftest or plugin starts one at import or at session start. The header line says so, and `incomplete = "trust"` relies on the recorded dependencies instead.

A test file is not even imported when every test it collected last time would be deselected, and neither the code its import ran nor the file itself changed. A file whose content changed in any way, for example a new test added to it, is imported.

`-k`, `-m`, `--lf` and `--deselect` still filter on top of the selection.

### Changed files that are not Python code

Every selecting run compares the files under the rootdir that are not Python code against the last record. It skips dot-directories, the `norecursedirs` directories, virtual environments, `__pycache__`, `ignore` and a built-in ignore list: `.coverage*`, `.test_durations`, `.DS_Store` and bytecode. For a changed, added or removed file:

1. If it is in the built-in full-run list or in `full_run_on`, everything runs. The built-in list covers files that change tests without being read through Python: `pytest.ini`, `pyproject.toml`, `tox.ini`, `setup.cfg`, `setup.py`, lock and requirements files (`uv.lock`, `poetry.lock`, `Pipfile.lock`, `requirements*.txt`, `constraints*.txt`), native code and its sources (`*.so`, `*.pyd`, `*.dylib`, `*.pyx`, `*.pxd`, `*.c`, `*.cc`, `*.cpp`, `*.h`, `*.rs`) and `*.pth`.
2. Every `file_triggers` entry whose key matches applies: `"all"` runs everything, a list of globs runs the tests in the matching test files, and `"recorded"` adds nothing.
3. Otherwise `unknown_files` decides: `"ignore"` (the default) leaves it to the recorded dependencies, which already select the tests that read the file or listed its directory, and `"all"` runs everything.

After a full run caused by such a change, tests that did not run (because of `-k`, for example) lose their entries and run as new next time. The same happens to tests that a `file_triggers` entry selected but that did not run.

## Configuration

Configuration goes in `pyproject.toml`, under its own table, which real pytest ignores:

```toml
[tool.pytest-rs.affected]
# Any change under these runs everything (adds to the built-in list above).
full_run_on = ["docker/**", ".python-version"]

# A changed file no test read and no rule matches: "ignore" (default) or "all".
unknown_files = "ignore"

# Treated as changed on every run: tests that depend on them always run.
always_changed = ["src/app/settings.py"]

# These tests always run.
always_run = ["tests/smoke/**"]

# Never looked at.
ignore = ["docs/**"]

# Tests that start a subprocess that cannot be seen into: "run" them always
# (default) or "trust" what was recorded.
incomplete = "run"

# What a test depends on in the import-time code (module-level statements and
# class bodies) of the modules it reaches: "module" (default), all of it, or
# "names", the statements binding the names its code uses (see below).
import_time = "module"

# A change under the key runs: a list of test globs, "recorded" (trust what tests
# were seen reading) or "all".
[tool.pytest-rs.affected.file_triggers]
"migrations/**" = ["tests/db/**"]
"data/**" = "recorded"
```

Globs are matched against rootdir-relative paths. `*` stays within one directory and `**` spans directories. An unknown key is a usage error.

## With `-n`, pytest-split and git

- **`-n`**: every worker selects on its own and records the tests it ran, and the controller writes one data file. The controller shows how many test files were not imported, not the per-test breakdown.
- **pytest-split** (`--splits`/`--group`, `--splitting-granularity`): the tests are selected first, and the groups divide the selected tests. Every shard computes the same selection from the same data.
- **git**: in a git checkout, each recorded file state also notes the commit it was the content of, when the file was tracked and unmodified. A later run skips reading the files git reports unchanged since that commit. Git only saves reading: untracked and ignored files, files that had uncommitted changes, states from commits that are not available, and everything outside a repository are compared by content.

## Keep application imports out of the conftest's top level

If a `conftest.py` imports your application at module level, the code that the import runs counts toward every test. That code is the module-level statements and class bodies. Then any edit to a module constant or a class attribute anywhere in the application runs everything:

```python
# conftest.py: every test now depends on all of app's import-time code
from app import create_app

@pytest.fixture
def client():
    return create_app().test_client()
```

Import inside the fixtures that need it instead. The import then runs while the fixture is set up, and its code is attributed only to the tests that use the fixture:

```python
# conftest.py
@pytest.fixture
def client():
    from app import create_app
    return create_app().test_client()
```

The default deliberately runs too much rather than too little here. Some tests depend on module-level values without calling any function of that module, so dropping this rule would miss them. `import_time = "names"` narrows it instead.

## Narrowing import-time code by name (`import_time = "names"`)

Experimental. With `import_time = "names"`, a test depends only on the module-level statements and class attributes whose names its code uses, not on all the import-time code of the modules it reaches:

- **Units.** Each module-level statement other than `def` and `class` belongs to the names it binds (`X = ...`, `import m`, `from m import x`, names bound inside `if` and `try` blocks). Statements binding no name form one more unit. A class body splits the same way into one unit per attribute and a header: decorators, bases, keywords, the names of its methods and nested classes, and attributes with dunder names. A statement also records how many earlier statements bound each name it reads, so moving a definition across a use changes the user.
- **What a test depends on.** The units binding the names that are used by the functions it ran (fixtures, the code they call, and code run while importing), by its test file's top-level code, and in turn by those statements (`Y = X * 2`, default arguments, decorators, base classes). Calling a method also adds its class's header. Names resolve to the module that binds them: `X` in its own module, `m.X` through `import m`, `X` through `from m import X`, and names re-exported by star imports (`__all__` included). A test still depends on every function it ran, as before.
- **Attributes of objects whose type is not followed** (`self.size`, `graph.nodes`) depend on every class attribute and module-level name of that name in the modules the test reaches.

Where a name cannot be followed, more runs:

- These classes count whole for the tests that use them: `Enum`, dataclass, `NamedTuple` and `TypedDict` classes, classes with decorators, keywords, a metaclass or `__init_subclass__`, and classes with a base other than a project class or `object`.
- Every test that reaches a module depends on these statements in it:
  - statements that bind no name;
  - statements that write anything other than a plain name (`m.X = 1`, `d[k] = v`, `setattr`);
  - statements that call anything other than the standard library or an installed package (`register(Handler)`, `X = make()`);
  - classes whose definition runs project or third-party code: a decorator or metaclass that is not the standard library's, a base whose `__init_subclass__` runs, a third-party base, or a class body that calls project code.
- A name the module does not bind (one provided by a module `__getattr__`, `globals()`, or `setattr` from elsewhere), `globals()` itself, and an attribute of a module with a computed name (`getattr(m, name)`, `vars(m)`) make the whole module a dependency.
- An attribute with a computed name on an object whose type is not followed (`getattr(obj, name)`, `obj.__dict__`, `vars(obj)`) depends on every attribute of every class the test touches: classes it uses by name, and classes whose methods it ran.
- Test files, `conftest.py` files and files matched by `always_changed` are not narrowed.

Only recording runs use the setting. Entries recorded under either setting can be used and merged together. It costs record time and data size. See [Known gaps](#known-gaps) for what it can miss.

## In CI

Record on the main branch, then select on pull requests against the newest recording. Every entry is checked against its own recorded checksums, so a recording from any earlier commit is usable. A pull request only runs more tests when the recording is older.

The data must come from the same Python and the same installed packages. Otherwise everything runs. So put the operating system and the Python version into the cache key.

```yaml
# .github/workflows/tests.yml
on:
  push:
    branches: [main]
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.13"
      - run: pip install -e . pytest-rs

      # The newest recording from main (any commit will do).
      - uses: actions/cache/restore@v4
        with:
          path: .pytest-rs-affected
          key: affected-${{ runner.os }}-py3.13-${{ github.sha }}
          restore-keys: affected-${{ runner.os }}-py3.13-

      # main: run everything and record. Pull requests: run the affected tests.
      - run: pytest-rs --affected=record
        if: github.event_name == 'push'
      - run: pytest-rs --affected=select
        if: github.event_name == 'pull_request'

      - uses: actions/cache/save@v4
        if: github.event_name == 'push'
        with:
          path: .pytest-rs-affected
          key: affected-${{ runner.os }}-py3.13-${{ github.sha }}
```

### Sharded runs

When the suite is split over several jobs with `--splits`/`--group`, each shard records its own group into a separate file, and a final job merges them. Merging keeps the newer entry of each test, so shards that started from the same restored data merge correctly in any order. `--affected-merge` takes the files as arguments, writes `--affected-data`, and runs no tests.

```yaml
jobs:
  test:
    strategy:
      matrix:
        group: [1, 2, 3, 4]
    steps:
      # ... checkout, Python, install as above ...
      - run: >
          pytest-rs --affected=record --splits 4 --group ${{ matrix.group }}
          --affected-data affected-${{ matrix.group }}
      - uses: actions/upload-artifact@v4
        with:
          name: affected-${{ matrix.group }}
          path: affected-${{ matrix.group }}

  merge-affected:
    needs: test
    runs-on: ubuntu-latest
    steps:
      # ... checkout, Python, install ...
      - uses: actions/download-artifact@v4
        with:
          pattern: affected-*
          merge-multiple: true
      - run: pytest-rs --affected-merge affected-1 affected-2 affected-3 affected-4 --affected-data .pytest-rs-affected
      - uses: actions/cache/save@v4
        with:
          path: .pytest-rs-affected
          key: affected-${{ runner.os }}-py3.13-${{ github.sha }}
```

With `--affected=select` on pull requests, the tests are selected first and the groups divide the selected tests.

## Known gaps

`--affected` can miss a test in these cases. Use `always_changed`, `always_run`, `file_triggers` or `full_run_on` to cover them.

- **Code that did not run in the recorded run.** A function a test did not call is not its dependency. A change to a function the test did call still reruns it.
- **Reads without calls.** A test that only reads a constant or class attribute of a module first imported by an earlier test, without calling anything in that module.
- **Mocks and autospec.** A test that patches `f` never runs `f`, so a change to `f` does not rerun it, even one that breaks `create_autospec`.
- **State outside the process**: databases, environment variables, the clock, the network, files written by other tests, test order. Running a subset can expose a hidden dependency between tests as a failure, which is visible rather than missed.
- **Code not compiled from a file under the rootdir**: `exec` of strings, generated code, C or Cython extensions built in-tree.
- **Files read by non-Python code**, such as a C library parsing a config file. (Tests that start a program other than Python always run, unless `incomplete = "trust"`.)
- **Edits during the session to files a Python subprocess used.** A subprocess reports which files it ran and read, and they are hashed when the session ends rather than when the subprocess read them. If one is edited in between, the record holds the edited version, so a later run does not select the test for that edit.
- **Installed packages**: their contents are not tracked, but any change in the set of installed packages or their versions means a full run.
- **Modules the import hook does not see run.** Module executions are seen through a finder placed first on `sys.meta_path`, and the gaps below come from that. In each case, the module's code still counts toward whatever context runs it, as it always did. What is lost is only its own record, so a test file that reaches the module through a cache hit, or through another module, can miss a change to its import-time code:
  - modules found by a finder that another plugin or conftest inserted ahead of it later;
  - modules whose loader class cannot be subclassed (a loader implemented in C, or one without an instance `__dict__`);
  - a module imported from the cache inside a function body, not at the top level of the importing module;
  - a module no longer in `sys.modules` when the session ends (removed or replaced by a test), whose import statements cannot be resolved to files then.

  A module executed with `importlib.util.spec_from_file_location` plus `exec_module`, `runpy` or `exec` is not tracked as a module of its own. Its code counts toward the context that runs it, which is the test, or the module whose import ran it.

With `import_time = "names"`, these are not followed either:

- **Module objects obtained at run time** (`importlib.import_module`, `sys.modules`, a module passed as an argument) and read with a computed attribute name. A literal attribute name is covered by the rule for attributes of objects whose type is not followed.
- **An edit that makes a module fail to import.** Only the tests that use the edited name run. They fail, or the run stops at a conftest that imports the module. The other tests that import the module are not selected until something they depend on changes.

It can also run more than needed:

- Application code imported by a conftest makes every test depend on the application's import-time code (see above).
- Code on background threads counts toward whichever test runs at the time.
- Recording with `--cov` re-arms coverage's line events at every test, which costs about as much as `--cov-context=test`.
