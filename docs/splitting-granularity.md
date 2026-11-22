# Splitting shards before import (`--splitting-granularity=file`)

Status: implemented as an opt-in option, `--splitting-granularity={test,file}`
(pytest-rs only; default `test`). The default `--splits`/`--group` behavior is
unchanged.

## Problem

The `pytest-split` layer (`crates/pytest-rs-split`) assigns tests to groups in
`pytest_collection_modifyitems`, i.e. after every test module of the session
has been imported and collected. Each shard (and, under `-n`, each worker of
each shard) therefore pays the import and collection cost of the whole suite
and then throws most of the items away. For suites whose modules are expensive
to import, that cost is paid `splits × workers` times.

## Why the default cannot move before import

The default algorithms place individual **tests**:

- `duration_based_chunks` walks the collected items in order and cuts the
  list into contiguous chunks of roughly equal duration;
- `least_duration` places the longest test into the currently shortest group;
- a test without a recorded duration counts as the average duration of the
  recorded tests that are *present in this run*.

All three depend on the final item list (parametrization, `-k`/`-m`/
`--deselect`, items added by plugins), which only exists after import. Two
tests of the same file regularly end up in different groups. No pre-import
computation can reproduce that assignment, so the default stays item-level and
post-collection, exactly as today (and as upstream pytest-split).

## `--splitting-granularity=file`

With `--splitting-granularity=file`, the unit of splitting is the test **file**:

1. After `pytest_configure` and the collect-ignore filters, and before any
   test module is imported, the plugin receives the ordered list of test files
   the session would collect (the `python_files` scan of the arguments).
2. Each file's duration is the sum of the `.test_durations` entries whose node
   id starts with that file's node id followed by `::` (or equals it).
3. A file with no recorded duration counts as the average duration of the
   files in this run that have one (1.0 if none do) — the item-level rule,
   applied per file.
4. The algorithm runs on those file units unchanged:
   `duration_based_chunks` keeps the files in collection order,
   `least_duration` sorts them by duration (ties by node id). Without an
   explicit `--splitting-algorithm` (on the command line or in `addopts`),
   file granularity uses `least_duration` — unlike test granularity, whose
   default stays `duration_based_chunks` (see below).
5. Only the files of the selected group are imported and collected, by the
   serial session and by every `-n` worker alike.

`pytest_collection_modifyitems` then keeps items whose file belongs to the
group, drops items whose file belongs to another group, and splits the
remaining items — those whose file was not in the scan, e.g. doctests of
non-test modules under `--doctest-modules` or items of `pytest_collect_file`
collectors for non-Python files — with the item-level algorithm, as today.
Every shard sees the same set of such files, so each of those items still runs
in exactly one group.

### Differences from the default

- Assignment granularity is the file: all tests (and parametrizations) of a
  file run in the same group. Groups are less evenly balanced when a few files
  dominate the total duration.
- Durations are computed before `-k`/`-m`/`--deselect` and node-id arguments
  narrow the selection, so a file counts with all of its recorded tests even
  when only some of them will run.
- Without any recorded durations, groups get roughly equal numbers of
  *files*, not of tests.
- The "estimated duration" line reports the file-level estimate.
- The default algorithm is `least_duration`, not `duration_based_chunks`:
  the latter cuts the file list into contiguous runs, and with a few long
  files a run can overshoot and leave the last group empty. It is still used
  when given explicitly.

### What is still paid for every file

The test-file scan (directory walk, no import) and, in a serial run, the
assertion-rewrite cache prefetch, which happens before `pytest_configure`
and therefore before the plugin knows the group. Under `-n` the workers
prefetch only their group's files. Conftests reachable from the arguments are
loaded as before.

`--store-durations` is unaffected: it still records per-test durations, which
both modes read.
