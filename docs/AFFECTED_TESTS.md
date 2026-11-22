# Affected-test selection (testmon-style) — design

Status: **design draft. Nothing is implemented yet.**

This is an opt-in mode that runs only the tests affected by code that changed since a recorded run, in the spirit of [pytest-testmon](https://github.com/tarpas/pytest-testmon). Without the new options nothing changes. The default run, the conformance suites and every existing option behave exactly as they do today.

Working name for the feature and its options: **`--affected`**. §9 explains why it is not `--testmon`.

---

## 1. How pytest-testmon works (summary)

Reference: pytest-testmon at `c86f995` (`DATA_VERSION = 13`).

### Per-test coverage

- testmon drives a coverage.py `Coverage` object. `include` is set to the rootdir and `omit` to the stdlib and site-packages.
- At every `pytest_runtest_protocol` it calls `cov.switch_context(nodeid)`. One context covers setup, call and teardown, so fixture code counts toward the test.
- Data is pulled out in batches of 250 tests, as `{file: set(lines)}` per test. Coverage is then erased and restarted.
- Coverage starts lazily at the first test. **Import-time code that runs during collection is never attributed to any test.** The only stand-in is a fake "line 1 of the test's own file" entry.
- Under xdist, workers attach the data to the teardown report and the controller writes the DB.
- Subprocess coverage is not implemented.

### Blocks and checksums (`process_code.py`)

- Each file is `ast.parse`d and the AST is dumped without positions. Whitespace and comments are ignored. Docstrings and other string literals count.
- Each function or method *body* is its own block.
- Everything else goes into one *module block* that spans the whole file: top-level statements, class bodies, decorators, signatures, defaults and annotations.
- A block's checksum is `crc32(str(ordinal) + ":" + dump)`. Because of the ordinal prefix, adding or removing a function changes the checksum of every block after it.
- A test's fingerprint for a file is the set of checksums of the blocks that contain a line it executed. Any file the test touched contributes its module block.
- Match rule: a test is unaffected iff every checksum it recorded still exists among the file's current block checksums.

### `.testmondata` (SQLite)

| Table | Contents |
|---|---|
| `environment` | name, installed-packages string, Python version |
| `test_execution` | test name, duration, `failed`, `forced` |
| `file_fp` | filename, packed int32 checksum array, fsha, mtime (deduplicated) |
| `test_execution_file_fp` | many-to-many link between `test_execution` and `file_fp` |
| `metadata` | savings statistics |

### Selection

- The current blob SHA of each known file comes from `git ls-files --stage`. If the file is modified, or git is absent, it is hashed from disk.
- Only files whose SHA changed are re-parsed. Affected tests come out of a temp-table join.
- Previously failed tests always rerun. New tests get an impossible `"0match"` fingerprint, so they always run.
- Whole stable files are skipped in `pytest_ignore_collect`, and stable items are deselected in `pytest_collection_modifyitems`. Selected tests run fastest module first.
- `-k`, `-m`, `--lf` and explicit node ids disable selection unless `--testmon-forceselect` is given.
- Any change in the installed package set or the Python version creates a fresh environment, which means a full run.

### Known weaknesses

These are visible in the code, not only in the README.

- Import-time and collection-time code has no per-test attribution.
- The module block couples nearly everything. Any edit to a class attribute, decorator or signature reruns every test that touched that file.
- Because of the ordinal prefix, inserting or reordering functions invalidates every block after the edit.
- Non-Python files (data, templates, migrations, config) are not tracked at all.
- Subprocesses are not supported.
- Mocked or autospecced targets are invisible.
- Only code that actually ran in the recorded run is a dependency.
- A flaky failure keeps a test selected until it passes.

---

## 2. Requirements and non-goals

Requirements:

- **Opt-in, zero default impact.** No sys.monitoring tool, audit hook, data file or behavior change unless `--affected*` is given. The conformance CI stays exactly as it is.
- **Safe by default.** When a test's dependencies are unknown or untrustworthy, the test runs. Running too much is acceptable. Missing a test is a bug, unless the case is in the documented gap list (§8).
- **Selection before import.** In a select run, unaffected test files must not be imported. Import time is a large share of a small selected run.
- **CI-friendly data.** The data file is recorded on the main branch. It must be usable for a PR head at any distance from that commit, and it must stay correct after partial (selected) runs are merged into it.
- **Works with `-n` and pytest-split.** Selection happens first. Splitting and distribution operate on the selected set.

Non-goals:

- Compatibility with `.testmondata` or with the `--testmon*` flags. This is a pytest-rs feature, not a reimplementation of the plugin (§9).
- Tracking changes inside installed third-party packages. The environment fingerprint (§5.1) covers them coarsely.

---

## 3. How to record dependencies — options compared

A test *depends on* a code unit when a change to that unit could change the test's outcome. The candidate ways to record that:

| # | Method | Record-run overhead | Select-only run | Precision (missed / over-run) | Effort | With `--cov` | With `-n` |
|---|---|---|---|---|---|---|---|
| A | **sys.monitoring `PY_START`, function units.** One callback per (code object, test): DISABLE, then `restart_events()` at each test boundary | Low. One Rust callback per distinct function per test, plus re-instrumentation after each restart. Estimated at a few ms per test; **to be measured** | Same for all methods: load the data, hash the changed files | Missed: only what never runs as a function (constants read without a call, mocks). Over-run: any edit inside a function reruns all of its callers | Medium | Separate tool id. `restart_events()` is global, so it also re-arms cov's DISABLEd LINE events once per test. The cost is about what `--cov-context=test` costs today | Per-worker collector. Deps travel through the existing `pytest_worker_dump/load` |
| B | sys.monitoring `LINE`, mapped to testmon-style blocks | Higher. One callback per distinct line per test, the same as `--cov-context=test` | same | About the same as A. Blocks are function-sized anyway, so line data adds no precision once it is mapped back to blocks | Medium | Could reuse cov's context mode directly | Same as A |
| C | `sys.setprofile` / `sys.settrace` | High. setprofile fires on every call and return with no DISABLE. settrace costs 2–5x on the whole suite | same | Same as A (setprofile) or B (settrace) | Low | Conflicts with debuggers and other tracers. Independent of cov | Same as A |
| D | Reuse coverage.py (CTracer or the sysmon core) | High. coverage.py's sysmon core does not support dynamic contexts, so per-test contexts fall back to the settrace CTracer | same | Same as B | Low–medium, plus a hard dependency on `coverage` | **Clashes** with the native cov, which does not use coverage.py for measurement | Needs its own data shipping |
| E | Import hooks only (module units) | Near zero. One meta-path or audit hook per import | same | Over-run: editing a module reruns every test whose file imports it, even transitively. Missed: a module already in `sys.modules` before the test file was imported is invisible unless it is resolved statically | Low | Independent | Easy |
| F | Static analysis only (import graph and call graph from the ruff AST) | **None. No record run is needed** | Parse all project files. Fast in Rust, roughly ~100 ms per few thousand files; to be measured | The import graph has the same over-run as E. A static call graph cannot see Python's dynamic dispatch, DI, fixtures looked up by name, `getattr`, frameworks or string imports, so it **misses** too much to be the sole source | Import graph: low. Call graph: high | Independent | Trivial |
| G | **Hybrid: A for test-time code, plus dynamic import-time attribution, plus a static import graph for what the runtime cannot see** | A's cost, in record runs only | Same. The static part is computed only for changed files | Best of the set (rules in §4.3) | Medium–high, staged | Same as A | Same as A |

**Recommendation: G, staged.** PR 1 ships the A core plus the import-time rules in §4.3.

- **Function units, not lines (A over B).** testmon's own blocks are function bodies, so B buys nothing once lines are mapped back to blocks. A needs far fewer callbacks: one per function per test instead of one per line.
- **sys.monitoring, not C or D.** The project already relies on sys.monitoring and requires Python ≥ 3.13. DISABLE plus restart gives once-per-test semantics cheaply. coverage.py's per-context path is the slow settrace tracer.
- **Not E or F alone.** Module granularity reruns too much in any codebase with a shared core module. A static call graph misses dynamic code, which fails in the wrong direction (§2).
- **Where static analysis still earns its place.** It covers imports that are cache hits (§4.3). It also makes selection *before import* possible: the per-file decision comes from stored data and ruff-parsed checksums, without executing anything.

**Open choice within A.** A variant keeps `PY_START` armed and dedupes in Rust instead of calling `restart_events()`. It costs one callback per *call* rather than one per distinct function per test. PR 1 measures both variants on public suites and keeps the cheaper one. The rest of the design does not depend on which one wins.

---

## 4. Recording

### 4.1 Code units and checksums

Units are computed in Rust from the ruff AST. The same code runs at record time and at selection time. Each Python file splits into:

- **Function units.** Each `def` or `async def`, keyed by its qualified name. The checksum covers the decorators, signature and body. Nested `def`s and `class`es are replaced by a placeholder because they are units of their own.
- **Class units.** The class header plus its body statements, excluding method bodies and nested class bodies.
- **One module unit.** The top-level statements other than `def` and `class`, which are units of their own.

Consequences, which differ from testmon:

- Adding a new function to a module changes no existing unit. Nothing reruns, which is correct because no recorded test called it.
- A unit's identity is its qualname, not an ordinal. Inserting or reordering functions invalidates nothing.
- Duplicate qualnames, such as `if X: def f(): ... else: def f(): ...`, are merged into one unit whose checksum covers every definition.

Checksums:

- A unit's checksum is a stable hash of ruff's `ComparableStmt`, which carries no positions. Formatting and comments are ignored.
- Docstrings count, as in testmon, because they can carry behavior (doctest, CLI help, …). Ignoring them could become an option later.
- The hash must be a fixed algorithm, for example xxh3 or blake3 over a canonical byte stream. It must not be `std`'s `DefaultHasher`, whose algorithm is unspecified across Rust releases.
- The data header records a checksum-scheme version and the ruff tag. A mismatch means a full re-record, never a silent mismatch.

Mapping a code object to its unit at record time:

- The collector stores `(co_filename, co_qualname, co_firstlineno)` and resolves it to a unit id once per code object, not once per event.
- `co_qualname` maps to the unit directly. `<locals>` segments are kept.
- Lambdas and genexprs map to the enclosing function. Comprehensions are already inlined since 3.12.
- A class-body code object maps to its class unit, and `<module>` maps to the module unit.

**Implied dependencies.** A dependency on a function unit also adds every enclosing class unit and the module unit of that file. This matches testmon's rule that any touched file contributes its module block, but without the ordinal and signature coupling.

### 4.2 Test-time recording (sys.monitoring)

A new `AffectedCollector` follows the same pattern as `crates/pytest-rs-cov/src/collector.rs`.

**Setup.**

- It claims its own tool id with `use_tool_id`, trying free ids the way cov tries `[1, 3, 4, 5]`. It never uses 0, the debugger's id.
- Only the global `PY_START` event is enabled.
- The `py_start` callback filters files the same way cov does: the file must be under the rootdir or the configured sources, and outside site-packages, the stdlib and the embedded shim. It inserts the unit id into the *current context's* set and returns `DISABLE`.

**Contexts.**

| Context | Active |
|---|---|
| `test:<nodeid>` | from `pytest_runtest_setup` through teardown |
| `fixture:<scope>:<baseid>::<name>` | while a fixture with scope above function is set up or finalized (§4.4) |
| `collect:<file>` | while a test module is imported |
| `collect:global` | everything else in the session: plugin and conftest import, collection hooks, `pytest_configure`, `pytest_sessionstart`, … |

**Context switches.**

- Each switch calls `sys.monitoring.restart_events()`, so every function fires once more under the new context.
- The cost is re-instrumenting the code objects that execute again, plus one Rust callback per distinct function per context.
- `restart_events()` is process-global. When `--cov` is also on, cov's LINE events fire once again per line per test.
  - Coverage stays correct, because hits are a set.
  - The cost becomes roughly what `--cov-context=test` costs today.
  - PR 1 documents this. A later PR may let the two share one collector, with cov's contexts deriving units from lines.

**Threads.**

- sys.monitoring is process-wide. Code that runs on a background thread during test T is attributed to T, which over-attributes and is therefore safe.
- A thread started by a session fixture that keeps running across tests spreads its deps over whichever tests run concurrently. That is also safe, but noisy. It is documented, not fixed.

### 4.3 Import-time and collection-time dependencies

Import-time code (module units and class units) runs once per process. Per-test recording alone would attribute it only to the first test that imports the module. Four rules cover it:

1. **Global collection context.** Everything executed in `collect:global` becomes a dependency of *every* test. That includes `-p` plugins, conftests, `pytest_configure` and `pytest_collection_modifyitems`. A change there means a full run.
   - Refinement (later): scope conftest import-time code to the tests under that conftest's directory.
2. **Test-file collection context.** Everything executed while importing test file X becomes a dependency of every test in X. That is X's own module code plus every project module that X imports *for the first time*.
3. **Static direct imports (cache hits).** A module already in `sys.modules` before X was imported is invisible to rule 2.
   - X's top-level `import` and `from … import` statements are resolved statically, with ruff and the rootdir/`sys.path` rules. Imports inside `if TYPE_CHECKING` are included; the over-run is harmless.
   - Their module units become dependencies of X's tests.
   - Refinement (later): a *dynamic import graph*. The collector keeps a stack of `<module>` code objects, which records which module's execution triggered which import. That gives a transitive closure instead of direct imports only.
4. **Lazy imports inside a test.** These are attributed to that test, since it runs the module code. A later test that uses the cached module is covered by implied dependencies (§4.1) as soon as it calls anything in it.
   - Remaining gap: a later test that only reads a constant of a lazily imported module, without calling any function in it (§8).

Together these give at least testmon's coverage, since testmon attributes import-time code to nobody.

#### Application code imported by a conftest

A conftest that imports the application (`from app import models`, `import mypkg as pkg`) runs the application's import-time code in `collect:global`. By rule 1 every module and class unit executed there becomes a dependency of every test, so editing a single module constant or class attribute anywhere in the application means a full run.

- **Default: unchanged.** Rule 1 stays as it is. Its only job beyond conftest code itself is to cover reads without calls: a test calls `N.f`, which reads `M.LIMIT`, and the test runs nothing in `M`. Implied dependencies (§4.1) do not cover that case.
- **Workaround, documented with PR 2.** Move the application import from the conftest's top level into the fixtures that need it. The import then runs under a fixture or test context (§4.4, rule 4), and its import-time code is attributed to the tests that use it. No implementation change is needed.

Measured alternatives, on networkx 3.6.1 (6890 tests; its conftest does `import networkx as nx`). 30 module constants and 30 class attributes were sampled from the 81 and 85 candidates and edited one at a time:

| Rule | Module constant: tests selected | Class attribute: tests selected |
|---|---|---|
| Rule 1 as designed (PR 1) | all 6890 in 30 of 30 | all 6890 in 30 of 30 |
| Import-time code split per module, re-attached through the static-import closure of each test's roots (its file and the files whose code it ran) | all 6890 in 30 of 30 | all 6890 in 30 of 30 |
| Global names referenced by the functions the test ran (estimate) | median 50, max 6699 | median 32, max 6734 |

- **The static-import closure is rejected.** `networkx/__init__.py` re-exports every subpackage, and every test file imports the package. The closure therefore reaches nearly every module from every test. A package that re-exports everything from `__init__` gets nothing from it.
- **Name-based narrowing (opt-in, PR 4).** A test depends on a module-level or class-level statement only if a function the test ran references a name that the statement defines. The dependency follows names through other import-time statements to a fixed point, for example `Y = X * 2`, a default argument, a decorator or a base class.
  - The estimate above matched bare identifiers without resolving scopes. Its high maxima are partly name collisions, such as `value` and `backends`, and partly real shared state, such as `Graph._node`, which most tests use.
  - The estimate also missed a case. `gml.Pattern.COMMENT_WHITESPACE` is an `Enum` member that is reached only through `Pattern(i)`, so no function names it. The name rule selected 0 tests for it, although every test that tokenizes a GML comment depends on it.
  - It ships disabled by default, behind an option, with these safety rules:
    - Names are resolved to module scope: `X` defined in `M`, `M.X` through `import M`, and `X` through `from M import X`. A bare identifier match across modules is not enough.
    - A class attribute of an `Enum`, a dataclass, a `NamedTuple`, a `TypedDict` or any class whose body is introspected (a class with a metaclass or `__init_subclass__`) makes the whole class unit the dependency, not the single name.
    - A name the resolver cannot follow falls back to the module unit: star imports, `getattr` or `setattr` with a non-literal name, `globals()` or `vars()`, `__getattr__` on a module, and attribute access on an object whose type is unknown.
    - Writes to module attributes from elsewhere (`M.X = ...` outside `M`) count as definitions of `M.X`.

### 4.4 Fixtures with scope above function

A session, package, module or class fixture runs its code once, under whichever test first requests it. On its own, that test would carry all of the fixture's deps.

The fix:

- While such a fixture is being set up or finalized, the context is `fixture:<scope>:<baseid>::<name>`.
- Each test records the names of the higher-scope fixtures in its closure, autouse included; these are known at setup time.
- A test's effective dependencies are its own set plus the sets of all those fixtures.
- Finalizer code runs at scope end, under some later test, so for the same reason it is attributed to the fixture.

This needs a native hook pair around fixture setup and finalization. Today only `pytest_fixture_setup` (firstresult) exists. Core gains a `pytest_fixture_post_setup` observer and a finalizer bracket.

### 4.5 Non-Python files and directories (`sys.addaudithook`)

In record runs only, a Rust audit hook watches these events:

- **`open`.** A file opened for read under the rootdir becomes a file dependency of the current context, checksummed by content hash. `.py`/`.pyc` files are skipped, and so are files opened by the import system: `io.open_code` also raises `open`, and those calls are filtered by suffix and by an import-in-progress flag.
- **`os.listdir` / `os.scandir`.** The result is a directory-listing dependency: a hash of the sorted entry names. This catches discovery code such as migration directories, `pkgutil.iter_modules` and plugin folders.
- **`subprocess.Popen`, `os.posix_spawn`, `os.exec*`.** The test is marked *deps incomplete*, so by default it always runs. This is configurable (§6).
  - A later PR can record inside Python subprocesses by reusing cov's `.pth` + `_child.py` mechanism.

Audit hooks cannot be removed once added. That is acceptable because the hook exists only in record runs. It filters on the event name in Rust before doing any work.

### 4.6 What is stored per test

- the nodeid
- the last outcome, `failed`. A failed test reruns until it passes
- the duration, used for ordering and by pytest-split
- a set of `(unit id, checksum at record time)` pairs
- a set of `(file or dir id, content hash at record time)` pairs
- the names of its higher-scope fixtures
- a `deps_incomplete` flag

Storing the checksum *per test* is what makes partial runs safe to merge. Each test's entry is valid on its own, whenever the other entries were recorded. This is exactly as in testmon.

---

## 5. Data file and CI operation

### 5.1 Format

The data file is `.pytest-rs-affected` in the rootdir. `--affected-data=PATH` overrides the location.

- **Layout.** One versioned binary file, loaded fully into memory.
  - Strings are interned: files, qualnames, nodeids.
  - Dependency sets are deduplicated, as testmon does with `file_fp`. Many tests share a set, for example every parametrization of one function.
- **Size estimate.** 10k tests × 300 deps × 8 bytes is about 24 MB before deduplication and compression, and much less after. To be confirmed on a public suite.
- **Codec.** Chosen in PR 1, preferring no new heavy dependency: the existing `serde` plus a small binary or length-prefixed writer. If incremental writes prove necessary, the fallback is SQLite (`rusqlite`, bundled). At this size a full rewrite on every save is fine.
- **Writes are atomic:** a temp file, then a rename.

**Header contents.**

- format version, checksum scheme and ruff tag
- pytest-rs version
- Python `sys.version_info`, implementation and platform
- the **environment fingerprint**: a hash of `(name, version)` for every installed distribution, with configurable excludes
- the **base commit** (`git rev-parse HEAD`) and whether the tree was dirty
- the rootdir-relative path scheme

Any mismatch in format, scheme, Python or environment means a full run, and in record mode a full re-record. This is the same policy as testmon's environment table.

### 5.2 Change detection

1. **Find candidate changed files.**
   - If the header's base commit exists locally, take the union of `git diff --name-only <base>`, untracked files (`git ls-files --others --exclude-standard`) and worktree changes (`git diff --name-only`). This is cheap and bounded by the size of the diff.
   - Otherwise, stat and hash every file that appears in the data. For thousands of files that takes milliseconds to a few hundred ms in Rust.
2. **Confirm by content hash** against the hashes recorded per test. Correctness never depends on the base commit; it only speeds up step 1. A data file from any commit is usable, because each test is compared against its own recorded checksums.
3. **Recompute unit checksums** with ruff, for changed `.py` files only.
4. **Classify** every changed path with the rules in §6.

### 5.3 CI flow

**Main branch.**

1. Run `pytest-rs --affected=record`, a full run that records.
2. Save the data file to the CI cache under a key that includes the commit, for example `affected-<os>-<py>-<sha>` with restore prefix `affected-<os>-<py>-`. An artifact also works.
3. If the run is sharded with pytest-split, each shard records its own subset, and a final job merges them with `pytest-rs --affected-merge a b c -o out` (PR 3).

**Pull request.**

1. Restore the newest main cache. Any commit is acceptable (§5.2).
2. Run `pytest-rs --affected=select`. Plain `--affected` also records the tests it runs (§7).
3. Optionally, save the result under a PR-scoped key so the next push to the PR selects against it.

**Order of steps inside a run.**

1. Selection decides which test *files* to collect, before import.
2. Collection imports only those files.
3. Item-level selection drops unaffected items in the imported files.
4. `--deselect`, `-k`, `-m`, pytest-split's grouping and `-n` distribution run as usual.

Selection is deterministic for a given data file and tree. Every split shard therefore computes the same selected set and then takes its own group.

**Where this hooks into the engine.**

- The pre-import filter sits on the `files` list between `apply_collect_ignores` and `collect_files` (`engine/collect/mod.rs`), and in the worker's `precollect_all` (`worker.rs`).
- Conftests are still imported, because they define fixtures and hooks.
- Item-level filtering goes before pytest-split's `pytest_collection_modifyitems`. That needs an explicit ordering slot, because native plugins run in registration order, after the conftest hooks.

---

## 6. Configuration

Configuration lives in `[tool.pytest-rs.affected]` in `pyproject.toml`, not in `[tool.pytest.ini_options]`. Real pytest warns on unknown ini keys, and the project may still run real pytest somewhere.

```toml
[tool.pytest-rs.affected]
# Any change to these → run everything. Added to the built-in list below.
full_run_on = ["docker/**", ".python-version"]

# A changed non-.py file that is not a recorded dependency and matches no rule:
# "ignore" (default) or "all" (strict: full run).
unknown_files = "ignore"

# Treated as changed on every run → tests that depend on them always run
# (e.g. code that reads the clock, the environment, or external state).
always_changed = ["src/app/settings.py"]

# These tests always run, whatever changed.
always_run = ["tests/smoke/**"]

# A change under the key glob runs the listed test globs ("recorded" = trust the
# open()/listdir deps recorded by the audit hook; "all" = full run).
[tool.pytest-rs.affected.file_triggers]
"migrations/**" = ["tests/db/**"]
"src/**/templates/**" = "recorded"
"data/**" = "recorded"

# Changes here never trigger anything.
# ignore = ["docs/**", "**/*.md", ".github/**"]
```

Defaults lean toward running more:

- **Changed non-`.py` files.** These need care. A blanket full run would make every docs or CI edit a full run. Instead the default relies on what the audit hook can and cannot see:
  - **Recorded dependency.** If the file (or a directory listing that contains it) is a recorded dependency of some tests, those tests run. This holds for files read through Python's `open`/`io.open_code` and directories listed through `os.listdir`/`os.scandir`, at test time or at collection time. The hook is installed in `pytest_plugins_registered`, before any conftest is imported.
  - **Never read.** If no recorded run ever read the file through Python, no test's Python code depends on its content. It is ignored (`unknown_files = "ignore"`, the default).
  - **Built-in full-run list.** This covers files that influence tests without being read through Python's `open`, so the audit hook cannot see them. Any change to them means a full run:
    - pytest/pytest-rs configuration read by the Rust config loader: `pytest.ini`, `pyproject.toml`, `tox.ini`, `setup.cfg`. (`conftest.py` is `.py` and already covered.)
    - Environment definitions: `uv.lock`, `poetry.lock`, `Pipfile.lock`, `requirements*.txt`, `constraints*.txt`, `setup.py`. The environment fingerprint (§5.1) also catches installed-package changes, but only once they are installed.
    - Native code that is loaded with `dlopen` rather than `open`: `*.so`, `*.pyd`, `*.dylib`, and in-tree extension sources `*.pyx`, `*.pxd`, `*.c`, `*.cc`, `*.cpp`, `*.h`, `*.rs`.
    - `*.pth`.
  - `full_run_on` extends the built-in list. `unknown_files = "all"` restores the strict behavior: any unrecorded non-`.py` change means a full run.
  - What `"ignore"` can miss is already in §8: files read only by non-Python code (C libraries, subprocesses). Subprocess-spawning tests are `deps_incomplete` and always run anyway. Files read by an in-process C library (e.g. a native config parser) are the remaining gap, covered by `full_run_on` or `file_triggers`.
- **A new `.py` file** needs no extra action.
  - Nothing can import it without a change to an already-tracked file, which selects that file's dependents.
  - Code that discovers it through a directory listing is caught by the recorded dir dependency, which selects the tests that did the listing.
  - A new test file is collected because it is not in the data.
- **A deleted `.py` file** removes its units, so their dependents run.
- **A `.py` file that fails to parse** has all of its units treated as changed.
- **A `deps_incomplete` test** (subprocess, …) always runs. `incomplete = "run" | "trust"` switches this.
- **New tests** (not in the data) and **previously failed tests** run.
- **Explicit node ids** on the command line always run.
- **`-k`, `-m` and `--lf`** compose with selection as additional filters. testmon turns selection off instead. Composing is more predictable for a native option (decided).

---

## 7. Run modes

| Invocation | Selects | Records | Monitoring/audit overhead |
|---|---|---|---|
| (no option) | — | — | none (today's behavior) |
| `--affected=record` | no (runs whatever the other args select) | yes, merged into the data | yes |
| `--affected=select` | yes | no | **none**: data load, hashing, ruff on changed files |
| `--affected` (= `auto`) | yes (all when no data) | yes, for the tests that ran | yes |
| `--affected-data=PATH` | — | — | location of the data file |

Typical use: `record` on main CI, `select` on PR CI (cheapest) or `auto` (keeps a PR-scoped file fresh), and `auto` locally.

- If `select` has no usable data (missing file, or a version or environment mismatch), it runs everything and says so in the header line.
- Like testmon, it exits OK when no tests were collected only because everything was deselected.

**Expected overhead.** To be measured in PR 1 on public suites: the pinned conformance suites and a mid-size public application test suite. No numbers are claimed here.

- **record:** the A collector's cost from §3, plus the audit hook's per-event filter. It is dominated by tests that touch many distinct functions. It is negligible for I/O-bound tests and most visible on sub-millisecond unit tests.
- **select:** loading the data (tens of ms), hashing the candidate files and parsing the changed files with ruff. The savings come from the imports and tests that are skipped.

---

## 8. Known gaps (documented and accepted)

- **Code that did not run in the recorded run.** A function that was not called is not a dependency. This is inherent to any dynamic approach, and testmon has the same gap. Function granularity mitigates it in practice: a change to the calling function reruns the test.
- **Reads without calls.** A later test that reads a module constant or class attribute of a *lazily* imported module, without calling anything in that module (§4.3, rule 4).
- **Over-run from application code imported by a conftest** (§4.3, "Application code imported by a conftest"). This is not a miss: any edit to the application's import-time code means a full run. The documented workaround moves the import into fixtures. Opt-in name-based narrowing is planned for PR 4. Splitting import-time code per module and re-attaching it through the static-import closure was measured and rejected.
- **Mocks and autospec.** A test that patches `f` never runs `f`. A change to `f`'s signature that would break `create_autospec` does not rerun that test.
- **State outside the process.** DB contents, environment variables, the clock, the network, files written by other tests, test-order dependence. A subset run can expose a hidden inter-test dependency as a *failure*. That is visible, not a miss.
- **Code not compiled from a file under the rootdir.** `exec` of strings (`<string>`), generated code, and C or Cython extensions built in-tree. Use `always_changed` or `file_triggers` for these.
- **Files read by non-Python code** (C libraries, subprocesses). The audit hook does not see them.
- **Python subprocesses**, until PR 4. Until then these tests are marked `deps_incomplete` and run.
- **Installed third-party packages**, except through the environment fingerprint: any version change means a full run.

---

## 9. Placement in the codebase

**A feature, not a plugin reimplementation.** The README and `CLAUDE.md` fix the native plugin *reimplementations* at seven. pytest-testmon stays a third-party plugin that loads as-is through the `pytest11` shim. This feature is a pytest-rs option of its own, with its own flags, config table and data format.

It deliberately does **not** reuse `--testmon*`. Both would register the same options, so an installed pytest-testmon would collide with it. The implementation PR adds one sentence about this to the design-constraint text.

**Crate.** A new `crates/pytest-rs-affected`, behind a default-on cargo feature like the other bundled crates. At runtime it is inert unless `--affected*` is given. It implements `Plugin`: options, `pytest_plugins_registered`, runtest setup and teardown, `pytest_worker_dump/load`, and sessionfinish.

**Small, generic core additions.** None of these is specific to this feature.

- A pre-import *file filter* hook on the collected `files` list, in the main process and in the worker's `precollect_all`.
- An ordering slot so that a native plugin's `pytest_collection_modifyitems` can run before pytest-split's.
- Hooks that bracket fixture setup and finalization (§4.4).
- A collection-phase notification for "now importing test module X".

**Python.** No new embedded Python module is needed. If one turns out to be necessary, it follows the stdlib-only rule.

---

## 10. Staging

### PR 1: local, single process, selection after import

- **Options:** `--affected`, `--affected=record|select` and `--affected-data`.
- **Units and checksums (§4.1):** a ruff-based unit splitter with a stable hash.
- **Recording:** the A collector, with contexts for tests, higher-scope fixtures, test-file collection and global collection (§4.2–4.4), and import-time rules 1, 2 and 4 (§4.3).
- **Data file (§5.1):** header checks and an atomic merge-write.
- **Change detection:** content hash only, so no git is required.
- **Item-level selection** in `pytest_collection_modifyitems`:
  - rerun failed tests;
  - run new tests and explicit node ids;
  - turn exit status 5 into 0 when everything was deselected.
- **Config:** `full_run_on` (plus the built-in list), `always_changed`, `always_run`, `ignore` and `unknown_files`. In PR 1 no audit hook exists yet, so no non-`.py` file is a recorded dependency: PR 1 treats an unrecorded non-`.py` change outside the built-in list as `unknown_files = "all"` regardless of the setting, and the `"ignore"` default takes effect in PR 2 together with the audit hook.
- **`-n` with `--affected`** is a usage error ("not supported yet"), rather than a run that records wrong data.
- **Rust unit tests** for unit splitting and checksums:
  - formatting-only edits change nothing;
  - inserting a function changes nothing;
  - editing a body, a decorator or a class attribute changes exactly that unit.
- **End-to-end scenarios** under `tests/`, on small self-made projects: record, edit, select, then assert the selected node ids. They cover each rule in §4 and §6.
- **Measurement**, with alternating runs on public suites:
  - the overhead of `record` compared with no option;
  - the `restart_events` variant compared with the no-DISABLE variant (§3).

### PR 2: pre-import selection and non-Python dependencies

- The pre-import file filter (core hook).
- Static direct imports (§4.3, rule 3).
- `open` and `listdir` dependencies from the audit hook.
- `file_triggers`.
- `deps_incomplete` marking for subprocesses.
- Docs: the workaround for application code imported by a conftest, which is to move the import into fixtures (§4.3).

### PR 3: CI

- `-n` support: worker dump/load and selection on the worker side.
- The ordering slot before pytest-split.
- A git fast path based on the base commit.
- `--affected-merge`.
- A docs page with a GitHub Actions cache example.

### PR 4: refinements

- A dynamic import graph, for transitive import-time deps.
- Global deps scoped by conftest.
- Recording inside Python subprocesses, through cov's `.pth` child.
- A collector shared with `--cov`.
- Opt-in, default-off name-based narrowing of import-time dependencies, with the safety rules in §4.3.

### Decisions (review of this doc)

1. Option name: `--affected`.
2. `-k`, `-m` and `--lf` compose with selection as extra filters (testmon disables selection instead).
3. Unrecorded non-`.py` changes are ignored by default; a built-in list (config, lock files, native code) plus `full_run_on` triggers full runs, and `unknown_files = "all"` gives the strict behavior (§6).
4. The cargo feature is on by default; the feature itself stays inert unless `--affected*` is given.
5. Application code imported by a conftest keeps rule 1 by default. That is the safe over-run. The fixture workaround is documented in PR 2, name-based narrowing comes later as an opt-in, and the static-import closure is not adopted (§4.3).
