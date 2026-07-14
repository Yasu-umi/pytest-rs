use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use super::super::Engine;
use crate::hooks::HookContext;
use crate::python;

impl Engine {
    pub(crate) fn collect_extra_and_custom(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
        files: &[PathBuf],
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), String> {
        // --doctest-modules: also scan ALL .py files (not just test files) for doctests.
        if self.config.get_flag("doctest-modules") {
            let extra_py = crate::collect::collect_all_python_files(
                &self.config.invocation_dir,
                paths,
                self.config.get_flag("collect-in-virtualenv"),
                files,
            );
            if let Ok(py_config) = python::make_py_config(py, &self.config) {
                let import_mode = crate::collect::ImportMode::from_config(&self.config);
                for extra_file in &extra_py {
                    // Import the module and collect doctests.
                    if let Err(err) = python::collect_doctests_from_module(
                        py,
                        rootdir,
                        extra_file,
                        &py_config,
                        &mut self.session.items,
                        import_mode,
                        &self.session.initial_paths,
                    ) {
                        // Import errors skip the module with --doctest-ignore-import-errors.
                        if self.config.get_flag("doctest-ignore-import-errors") {
                            let nodeid = crate::collect::file_nodeid(
                                rootdir,
                                extra_file,
                                &self.session.initial_paths,
                            );
                            let longrepr = format!(
                                "unable to import module PosixPath('{}')",
                                extra_file.display()
                            );
                            python::record_collect_skip(py, &nodeid, &longrepr);
                            self.session.skipped_modules.push((
                                nodeid.clone(),
                                longrepr.clone(),
                                format!("{nodeid}:1"),
                            ));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(longrepr),
                                location: Some(format!("{nodeid}:1")),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        } else {
                            errors.push((extra_file.clone(), python::format_exception(py, &err)));
                        }
                    }
                }
            }
        }

        // Text files matching the glob (default: test*.txt) are always collected
        // even without explicit --doctest-modules or --doctest-glob flags, mirroring
        // upstream pytest's _is_doctest() behavior.
        let scan_text_files = true;
        if scan_text_files && let Ok(py_config) = python::make_py_config(py, &self.config) {
            let text_files =
                crate::collect::collect_doctest_textfiles(&self.config.invocation_dir, paths);
            for tf in text_files {
                // Skip files already collected in the explicit-file loop above.
                if files.contains(&tf) {
                    continue;
                }
                if let Ok(true) = python::is_doctest_textfile(py, &tf, &py_config)
                    && let Err(err) = python::collect_doctests_from_textfile(
                        py,
                        rootdir,
                        &tf,
                        &py_config,
                        &mut self.session.items,
                        &self.session.initial_paths,
                    )
                {
                    errors.push((tf.clone(), python::format_exception(py, &err)));
                }
            }
        }

        // Custom collectors: plugins like pytest-ruff / pytest-mypy collect
        // non-test files via pytest_collect_file -> pytest.File.collect().
        // Only walk the (broader) candidate file set when such a hook exists.
        if python::has_collect_file_hook(py, &self.session.py_hooks) {
            let candidate = crate::collect::collect_all_files(
                &self.config.invocation_dir,
                paths,
                self.config.get_flag("collect-in-virtualenv"),
            );
            let hooks = std::mem::take(&mut self.session.py_hooks);
            let result = python::collect_custom_files(
                py,
                rootdir,
                &candidate,
                &hooks,
                &mut self.session.items,
            );
            self.session.py_hooks = hooks;
            match result {
                Ok(collect_result) => {
                    if !collect_result.skipped.is_empty() {
                        let skipped_set: std::collections::HashSet<&PathBuf> =
                            collect_result.skipped.iter().map(|(p, _)| p).collect();
                        self.session
                            .items
                            .retain(|item| !skipped_set.contains(&item.path));
                        // A `pytest.skip()` raised inside `pytest_collect_file`
                        // makes the file's collectreport "skipped" upstream.
                        // Record each as a setup-phase skip so the summary line
                        // counts it ("N skipped") instead of collapsing to
                        // "no tests ran".
                        for (file, reason) in &collect_result.skipped {
                            let nodeid = crate::collect::file_nodeid(
                                rootdir,
                                file,
                                &self.session.initial_paths,
                            );
                            self.session
                                .collect_file_skips
                                .push((nodeid.clone(), reason.clone()));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(reason.clone()),
                                location: Some(format!("{nodeid}:1")),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        }
                    }
                    for (path, longrepr) in collect_result.errors {
                        errors.push((path, longrepr));
                    }
                    // pytest_collect_file handed back a bare File/Module (no
                    // real collect() override) for one of these files —
                    // trusting its stub's empty yield would silently collect
                    // zero tests, so scan it the same way a standard .py file
                    // is scanned instead.
                    let filters = python::NameFilters::from_config(py, &self.config);
                    let mode = crate::collect::ImportMode::from_config(&self.config);
                    for file in collect_result.native_fallback {
                        // A hook matching broadly (e.g. every ".py" file) may
                        // hand back a bare collector for a file the standard
                        // pipeline already scanned as a real test module, or
                        // for conftest.py itself (never a collectible module,
                        // and already imported through the dedicated
                        // conftest-loading path — re-importing it here would
                        // just trip the "import file mismatch" guard).
                        // Skip both: the file's own items already exist.
                        if files.contains(&file)
                            || file.file_name().and_then(|n| n.to_str()) == Some("conftest.py")
                        {
                            continue;
                        }
                        if let Err(err) = python::collect_module(
                            py,
                            rootdir,
                            &file,
                            &mut self.session.items,
                            &mut self.session.registry,
                            &mut self.session.py_hooks,
                            &filters,
                            mode,
                            &self.plugins,
                            &self.session.initial_paths,
                        ) {
                            errors.push((file, python::format_exception(py, &err)));
                        }
                    }
                }
                Err(err) => {
                    errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
                }
            }
            // Merge custom-file items in file-path order so they appear at
            // their alphabetical position, not always after all .py items.
            self.session
                .items
                .sort_by(|a, b| a.path.cmp(&b.path).then(a.lineno.cmp(&b.lineno)));
        }

        // Collection over: close its catching_logs phase.
        python::log_end_phase(py);
        Ok(())
    }

    /// Phase 9: expand parametrized-fixture closures, record closure
    /// fixturenames, apply node-id arg selection, and `--lf` filtering.
    pub(crate) fn finalize_items(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
    ) -> Result<(), String> {
        // Expand items over parametrized fixtures in their closure; plugins
        // first get to inject closure-affecting marks (anyio's usefixtures).
        let mut items = std::mem::take(&mut self.session.items);
        {
            let mut ctx = HookContext {
                py,
                session: &mut self.session,
                config: &self.config,
            };
            for plugin in &self.plugins {
                if let Err(err) = plugin.pytest_collection_preexpand(&mut ctx, &mut items) {
                    self.session.items = items;
                    return Err(python::format_exception(py, &err));
                }
            }
        }
        match python::expand_fixture_params(py, items, &self.session.registry) {
            Ok(expanded) => self.session.items = expanded,
            Err(err) => return Err(python::format_exception(py, &err)),
        }

        // Scope-based item reordering: when metafunc.parametrize(scope=...)
        // uses a scope higher than function, items must be reordered so
        // that the high-scope parameter value changes as infrequently as
        // possible (matching real pytest's reorder_items).
        reorder_items_by_param_scope(&mut self.session.items);

        // request.fixturenames must list the item's whole fixture closure
        // (transitive deps + autouse + `request` itself), not just its direct
        // params — plugins probe it (pytest-django: "transactional_db" in
        // request.fixturenames, pulled in transitively by
        // django_db_reset_sequences) and upstream's "--setup-plan"/
        // "--setup-show" display it verbatim (sorted(item.fixturenames),
        // unfiltered). Record the closure-only names as extra fixturenames
        // (display only; the fixtures themselves resolve through the
        // dependency chain via closure_for, which drops `request` since it
        // isn't a real fixture to set up).
        for item in &mut self.session.items {
            let mut direct: Vec<String> = item.fixture_names.clone();
            direct.extend(item.extra_fixture_names.iter().cloned());
            // Directly-parametrized argnames shadow a same-named fixture
            // (PseudoFixtureDef): keep them in the closure but don't expand
            // their dependencies.
            let ignore: std::collections::HashSet<String> =
                item.callspec.iter().map(|(name, _)| name.clone()).collect();
            let initialnames = self.session.registry.initial_names(&item.nodeid, &direct);
            let closure_names =
                self.session
                    .registry
                    .getfixtureclosure(&item.nodeid, &initialnames, &ignore);
            for name in closure_names {
                if !item.fixture_names.contains(&name) && !item.extra_fixture_names.contains(&name)
                {
                    item.extra_fixture_names.push(name);
                }
            }
        }

        // Node-id args ("file.py::TestCls::test_a") restrict collection to
        // matching items; unlike -k/-m this is not a deselection.
        enum ArgSel {
            Path(PathBuf),
            NodeId(String),
        }
        if paths.iter().any(|arg| arg.contains("::")) {
            let arg_sels: Vec<ArgSel> = paths
                .iter()
                .map(|arg| match arg.split_once("::") {
                    Some((file_part, rest)) => {
                        let path = self.config.invocation_dir.join(file_part);
                        let path = std::fs::canonicalize(&path).unwrap_or(path);
                        ArgSel::NodeId(format!(
                            "{}::{}",
                            crate::collect::file_nodeid(
                                rootdir,
                                &path,
                                &self.session.initial_paths
                            ),
                            rest
                        ))
                    }
                    None => {
                        let path = self.config.invocation_dir.join(arg);
                        ArgSel::Path(std::fs::canonicalize(&path).unwrap_or(path))
                    }
                })
                .collect();
            self.session.items.retain(|item| {
                arg_sels.iter().any(|sel| match sel {
                    ArgSel::Path(path) => item.path.starts_with(path),
                    ArgSel::NodeId(sel) => {
                        item.nodeid == *sel
                            || item
                                .nodeid
                                .strip_prefix(sel.as_str())
                                .is_some_and(|rest| rest.starts_with('[') || rest.starts_with("::"))
                    }
                })
            });
            // Emit "not found" error to stderr for NodeId args that matched nothing.
            for sel in &arg_sels {
                if let ArgSel::NodeId(nodeid) = sel {
                    let matched = self.session.items.iter().any(|item| {
                        item.nodeid == *nodeid
                            || item
                                .nodeid
                                .strip_prefix(nodeid.as_str())
                                .is_some_and(|r| r.starts_with('[') || r.starts_with("::"))
                    });
                    if !matched {
                        eprintln!("ERROR: not found: {nodeid}");
                        self.session.not_found_nodeids.push(nodeid.clone());
                    }
                }
            }
        }

        // --lf drops failure-free files (and non-failed top-level functions
        // of failed files) at collection time.
        if let Some(cache) = &mut self.cache {
            cache.filter_collected_items(
                rootdir,
                &self.config.invocation_dir,
                paths,
                &mut self.session.items,
            );
        }
        Ok(())
    }
}

/// A high-scope parametrization identity: items sharing one are grouped so
/// the fixture set up for that value is reused. Mirrors pytest's ParamArgKey
/// (argname, param_index, scoped_path/cls) — the boundary string folds the
/// path/class component.
type ParamArgKey = (String, usize, String);

/// High scopes, outermost first (pytest's HIGH_SCOPES).
const HIGH_SCOPES: [crate::fixture::Scope; 4] = [
    crate::fixture::Scope::Session,
    crate::fixture::Scope::Package,
    crate::fixture::Scope::Module,
    crate::fixture::Scope::Class,
];

pub(crate) fn next_lower_scope(scope: crate::fixture::Scope) -> crate::fixture::Scope {
    use crate::fixture::Scope;
    match scope {
        Scope::Session => Scope::Package,
        Scope::Package => Scope::Module,
        Scope::Module => Scope::Class,
        _ => Scope::Function,
    }
}

/// Order-preserving dedup (pytest's `dict.fromkeys`).
pub(crate) fn dedup_keys(keys: Vec<ParamArgKey>) -> Vec<ParamArgKey> {
    let mut seen = std::collections::HashSet::new();
    keys.into_iter()
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

/// Reorder items so higher-scoped parametrized fixtures change as
/// infrequently as possible — a faithful port of pytest's `reorder_items`,
/// recursively grouping by Session→Package→Module→Class param values.
pub(crate) fn reorder_items_by_param_scope(items: &mut Vec<crate::collect::TestItem>) {
    use crate::fixture::Scope;
    use std::collections::HashMap;

    if items
        .iter()
        .all(|item| item.max_param_scope == Scope::Function)
    {
        return;
    }

    // Per scope: each item's ParamArgKeys, and items grouped by argkey (in
    // item order). `items_by_argkey` is mutated during reordering to keep
    // lower-scope grouping consistent with higher-scope decisions.
    let mut argkeys_by_item: HashMap<Scope, HashMap<usize, Vec<ParamArgKey>>> = HashMap::new();
    let mut items_by_argkey: HashMap<Scope, HashMap<ParamArgKey, Vec<usize>>> = HashMap::new();
    for &scope in &HIGH_SCOPES {
        let mut abi: HashMap<usize, Vec<ParamArgKey>> = HashMap::new();
        let mut iba: HashMap<ParamArgKey, Vec<usize>> = HashMap::new();
        for (idx, item) in items.iter().enumerate() {
            let keys = dedup_keys(
                item.scope_sort_keys
                    .iter()
                    .filter(|(_, s, _)| *s == scope)
                    .map(|(arg, _, i)| (arg.clone(), *i, scope_boundary(&item.nodeid, scope)))
                    .collect(),
            );
            if !keys.is_empty() {
                for k in &keys {
                    iba.entry(k.clone()).or_default().push(idx);
                }
                abi.insert(idx, keys);
            }
        }
        argkeys_by_item.insert(scope, abi);
        items_by_argkey.insert(scope, iba);
    }

    let initial: Vec<usize> = (0..items.len()).collect();
    let ordered = reorder_items_atscope(
        &initial,
        &argkeys_by_item,
        &mut items_by_argkey,
        Scope::Session,
    );
    // Safety: only apply a full permutation (every item exactly once).
    if ordered.len() != items.len() {
        return;
    }
    let mut taken: Vec<Option<crate::collect::TestItem>> = items.drain(..).map(Some).collect();
    *items = ordered
        .into_iter()
        .map(|i| taken[i].take().expect("each index used once"))
        .collect();
}

pub(crate) fn reorder_items_atscope(
    items: &[usize],
    argkeys_by_item: &std::collections::HashMap<
        crate::fixture::Scope,
        std::collections::HashMap<usize, Vec<ParamArgKey>>,
    >,
    items_by_argkey: &mut std::collections::HashMap<
        crate::fixture::Scope,
        std::collections::HashMap<ParamArgKey, Vec<usize>>,
    >,
    scope: crate::fixture::Scope,
) -> Vec<usize> {
    use crate::fixture::Scope;
    use std::collections::{HashSet, VecDeque};

    if scope == Scope::Function || items.len() < 3 {
        return items.to_vec();
    }
    let items_set: HashSet<usize> = items.iter().copied().collect();
    let mut ignore: HashSet<ParamArgKey> = HashSet::new();
    let mut deque: VecDeque<usize> = items.iter().copied().collect();
    let mut items_done: Vec<usize> = Vec::new();
    let mut done_set: HashSet<usize> = HashSet::new();

    while !deque.is_empty() {
        let mut no_argkey_items: Vec<usize> = Vec::new();
        let mut no_argkey_set: HashSet<usize> = HashSet::new();
        let mut slicing_argkey: Option<ParamArgKey> = None;
        while let Some(item) = deque.pop_front() {
            if done_set.contains(&item) || no_argkey_set.contains(&item) {
                continue;
            }
            let argkeys = dedup_keys(
                argkeys_by_item[&scope]
                    .get(&item)
                    .map(|ks| {
                        ks.iter()
                            .filter(|k| !ignore.contains(*k))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            if argkeys.is_empty() {
                no_argkey_items.push(item);
                no_argkey_set.insert(item);
            } else {
                // pytest's popitem() pops the last key.
                let sk = argkeys.last().cloned().expect("non-empty");
                slicing_argkey = Some(sk.clone());
                let matching: Vec<usize> = items_by_argkey[&scope][&sk]
                    .iter()
                    .copied()
                    .filter(|i| items_set.contains(i))
                    .collect();
                for &i in matching.iter().rev() {
                    deque.push_front(i);
                    // Move i to the front of every argkey list it belongs to,
                    // across all high scopes (pytest's move_to_end last=False).
                    for &other_scope in &HIGH_SCOPES {
                        if let Some(keys) = argkeys_by_item[&other_scope].get(&i) {
                            let keys = keys.clone();
                            let scoped = items_by_argkey.get_mut(&other_scope).expect("scope");
                            for argkey in &keys {
                                if let Some(v) = scoped.get_mut(argkey) {
                                    v.retain(|&x| x != i);
                                    v.insert(0, i);
                                }
                            }
                        }
                    }
                }
                break;
            }
        }
        if !no_argkey_items.is_empty() {
            let reordered = reorder_items_atscope(
                &no_argkey_items,
                argkeys_by_item,
                items_by_argkey,
                next_lower_scope(scope),
            );
            for i in reordered {
                if done_set.insert(i) {
                    items_done.push(i);
                }
            }
        }
        if let Some(sk) = slicing_argkey {
            ignore.insert(sk);
        }
    }
    items_done
}

/// Extract the scope boundary key from a nodeid.
/// Session: "" (all items grouped together)
/// Module: "file.py" (everything before the first "::")
/// Class: "file.py::ClassName" (everything before the last "::" if there's
///        a class, otherwise the module)
pub(crate) fn scope_boundary(nodeid: &str, scope: crate::fixture::Scope) -> String {
    use crate::fixture::Scope;
    let module_path = || nodeid.split_once("::").map(|(m, _)| m).unwrap_or(nodeid);
    match scope {
        Scope::Session => String::new(),
        // Package scope groups by the module's directory.
        Scope::Package => module_path()
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default(),
        Scope::Module => module_path().to_string(),
        Scope::Class => {
            // file.py::Class::func[params] → "file.py::Class"
            // file.py::func[params] → "file.py" (no class)
            let base = nodeid.split('[').next().unwrap_or(nodeid);
            let parts: Vec<&str> = base.splitn(3, "::").collect();
            if parts.len() >= 3 {
                format!("{}::{}", parts[0], parts[1])
            } else {
                parts[0].to_string()
            }
        }
        Scope::Function => nodeid.to_string(),
    }
}
