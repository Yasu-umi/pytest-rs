#[allow(unused_imports)]
use super::super::*;
use crate::collect::{MarkData, TestItem};
use crate::fixture::FixtureRegistry;
use pyo3::types::{PyList, PyModule};
use std::path::Path;

use super::introspect::{
    async_flags, first_lineno, num_mock_patch_args, param_names_with_positional_only,
};
use super::utils::{collect_error, id_for_value, user_id_from_value};

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_test_items(
    py: Python<'_>,
    items: &mut Vec<TestItem>,
    nodeid_prefix: &str,
    module_name: &str,
    path: &Path,
    name: &str,
    func: &Bound<'_, PyAny>,
    cls: Option<&Bound<'_, PyAny>>,
    is_static: bool,
    marks: Vec<MarkData>,
    module: &Bound<'_, PyModule>,
    generate_hook: Option<&Bound<'_, PyAny>>,
    registry: &FixtureRegistry,
) -> PyResult<()> {
    let flags = async_flags(py, func)?;
    let (mut fixture_names, has_positional_only) = param_names_with_positional_only(py, func)?;
    // For non-static class methods, strip the first parameter (self/cls)
    // regardless of its name — pytest does the same in getfuncargnames.
    // When any positional-only parameter exists, the self/cls was already
    // excluded by param_names (it only collects POSITIONAL_OR_KEYWORD and
    // KEYWORD_ONLY), so skip the strip — matching pytest's getfuncargnames.
    if cls.is_some() && !is_static && !has_positional_only && !fixture_names.is_empty() {
        fixture_names.remove(0);
    }
    // @unittest.mock.patch-injected leading params are not fixture requests.
    let mock_args = num_mock_patch_args(py, func).min(fixture_names.len());
    if mock_args > 0 {
        fixture_names.drain(..mock_args);
    }

    // pytest_generate_tests: metafunc.parametrize calls become parametrize
    // marks, merged after the decorator-applied ones.
    let mut marks = marks;
    let fixture_names = fixture_names;
    // Transitive fixture closure: walk fixture deps so parametrize argnames
    // that reference indirect dependencies (e.g. fix2 when test_it requests
    // fix1 which depends on fix2) are recognized by validation.
    let test_nodeid = format!("{nodeid_prefix}::{name}");
    let mut closure_names: Vec<String> = fixture_names.clone();
    {
        let mut seen: std::collections::HashSet<String> = closure_names.iter().cloned().collect();
        // @pytest.mark.usefixtures names are part of the fixture closure
        // (upstream's initialnames = usefixtures + argnames), so
        // metafunc.fixturenames includes them for pytest_generate_tests.
        for mark in marks.iter().filter(|m| m.name == "usefixtures") {
            if let Ok(args) = mark.obj.bind(py).getattr("args") {
                for arg in args.try_iter().into_iter().flatten().flatten() {
                    if let Ok(s) = arg.extract::<String>()
                        && seen.insert(s.clone())
                    {
                        closure_names.push(s);
                    }
                }
            }
        }
        let mut i = 0;
        while i < closure_names.len() {
            if let Some(def) = registry.lookup(&closure_names[i], &test_nodeid) {
                for dep in &def.param_names {
                    if dep != "request" && seen.insert(dep.clone()) {
                        closure_names.push(dep.clone());
                    }
                }
            }
            i += 1;
        }
    }
    // Fixtures a generate hook appended (pytest-repeat's indirect step
    // fixture): set up so their request.param is consumed, but not injected
    // into the test signature.
    let mut extra_generated_fixtures: Vec<String> = Vec::new();
    if let Some(hook) = generate_hook {
        // metafunc.config (option.count etc.) and definition markers
        // (get_closest_marker) let plugin impls like pytest-repeat decide.
        let config = crate::python::proxies::existing_py_config(py).map(|c| c.into_bound(py));
        let mark_objs = pyo3::types::PyList::empty(py);
        for m in &marks {
            mark_objs.append(m.obj.bind(py))?;
        }
        let metafunc = py.import("pytest._metafunc")?.getattr("Metafunc")?.call1((
            func,
            closure_names.clone(),
            module,
            cls.map(|c| c.clone().unbind()),
            config,
            mark_objs,
        ))?;
        hook.call1((&metafunc,))?;
        for mark in metafunc.getattr("_parametrize_marks")?.try_iter()? {
            let mark = mark?;
            marks.push(MarkData {
                name: "parametrize".to_string(),
                obj: mark.unbind(),
            });
        }
        // A hook may append fixturenames (pytest-repeat adds its indirect
        // step fixture so its request.param is set up per repeat).
        let updated: Vec<String> = metafunc.getattr("fixturenames")?.extract()?;
        for name in updated {
            if !fixture_names.contains(&name) && !extra_generated_fixtures.contains(&name) {
                extra_generated_fixtures.push(name);
            }
        }
    }

    validate_parametrize_argnames(
        py,
        &marks,
        name,
        &closure_names,
        &extra_generated_fixtures,
        func,
        registry,
        &test_nodeid,
    )?;

    let variants = expand_parametrize(py, &marks, &test_nodeid, Some(func), registry)?;
    for variant in variants {
        let nodeid = match &variant.id {
            Some(id) => format!("{nodeid_prefix}::{name}[{id}]"),
            None => format!("{nodeid_prefix}::{name}"),
        };
        let mut item_marks: Vec<MarkData> = marks
            .iter()
            .map(|m| MarkData {
                name: m.name.clone(),
                obj: m.obj.clone_ref(py),
            })
            .collect();
        item_marks.extend(variant.extra_marks);
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: module_name.to_string(),
            func_name: name.to_string(),
            func: func.clone().unbind(),
            cls: cls.map(|c| c.clone().unbind()),
            is_coroutine: flags.is_coroutine,
            is_doctest: false,
            fixture_names: fixture_names.clone(),
            extra_fixture_names: extra_generated_fixtures.clone(),
            marks: item_marks,
            callspec: variant.params,
            fixture_params: variant.indirect_params,
            lineno: first_lineno(py, func),
            collector_class: String::new(),
            func_class: String::new(),
            py_node: None,
            max_param_scope: variant.max_param_scope,
            scope_sort_keys: variant.scope_sort_keys,
        });
    }
    Ok(())
}

pub(crate) struct ParamVariant {
    /// The "[...]" id suffix; None for unparametrized tests.
    id: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// indirect parametrize assignments: (fixture name, param index, value).
    indirect_params: Vec<(String, usize, Py<PyAny>)>,
    /// Marks attached via pytest.param(..., marks=...).
    extra_marks: Vec<MarkData>,
    /// Highest parametrize scope across all dimensions (for item reordering).
    max_param_scope: crate::fixture::Scope,
    /// Per non-function-scoped dimension: (argname, scope, 0-based set index).
    /// Feeds `reorder_items` so items sharing a high-scope param value group.
    scope_sort_keys: Vec<(String, crate::fixture::Scope, usize)>,
}

/// One parameter set (one `pytest.param`/value row) within a single
/// `@pytest.mark.parametrize` mark.
struct ParamSet {
    /// None hides the set from the test ID (pytest.HIDDEN_PARAM).
    id_part: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// indirect=True/[names]: the value parametrizes the same-named
    /// fixture (request.param) instead of being passed to the test.
    indirect_params: Vec<(String, usize, Py<PyAny>)>,
    extra_marks: Vec<MarkData>,
}

/// One `@pytest.mark.parametrize` mark's worth of parameter sets; stacked
/// marks become separate dimensions in the cartesian product.
struct Dim {
    sets: Vec<ParamSet>,
    scope: crate::fixture::Scope,
}

/// Expand stacked @pytest.mark.parametrize marks into the cartesian product
/// Validate that parametrize argnames are either function parameters or
/// known fixtures. Raises Failed(pytrace=False) like upstream's
/// `Metafunc._validate_if_using_arg_names`.
#[allow(clippy::too_many_arguments)]
fn validate_parametrize_argnames(
    py: Python<'_>,
    marks: &[MarkData],
    func_name: &str,
    fixture_names: &[String],
    extra_fixture_names: &[String],
    func: &Bound<'_, PyAny>,
    registry: &FixtureRegistry,
    test_nodeid: &str,
) -> PyResult<()> {
    let inspect = py.import("inspect")?;
    let all_params: std::collections::HashSet<String> = inspect
        .call_method1("signature", (func,))
        .and_then(|sig| sig.getattr("parameters"))
        .and_then(|params| {
            params
                .call_method0("keys")?
                .try_iter()?
                .map(|k| k.and_then(|v| v.extract::<String>()))
                .collect()
        })
        .unwrap_or_default();

    for mark in marks.iter().filter(|m| m.name == "parametrize") {
        let args = mark.obj.bind(py).getattr("args")?;
        if args.len()? == 0 {
            continue;
        }
        let argnames_obj = args.get_item(0)?;
        let argnames: Vec<String> = match argnames_obj.extract::<String>() {
            Ok(joined) => joined
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Err(_) => argnames_obj.extract()?,
        };
        let indirect_obj = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("indirect")
            .ok();
        let indirect_all = indirect_obj
            .as_ref()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false);
        let indirect_names: Vec<String> = indirect_obj
            .as_ref()
            .and_then(|v| {
                v.extract::<Vec<String>>()
                    .ok()
                    .or_else(|| v.extract::<String>().ok().map(|s| vec![s]))
            })
            .unwrap_or_default();

        for argname in &argnames {
            if argname == "request"
                || fixture_names.contains(argname)
                || extra_fixture_names.contains(argname)
                || all_params.contains(argname.as_str())
                || registry.lookup(argname, test_nodeid).is_some()
            {
                continue;
            }
            if !(indirect_all || indirect_names.iter().any(|n| n == argname)) {
                continue;
            }
            let msg = format!("In {func_name}: function uses no fixture '{argname}'");
            let failed_result: PyResult<PyErr> = (|| {
                let cls = py.import("_pytest.outcomes")?.getattr("Failed")?;
                let instance = cls.call1((&msg,))?;
                instance.setattr("pytrace", false)?;
                Ok(PyErr::from_value(instance))
            })();
            return Err(match failed_result {
                Ok(err) => err,
                Err(_) => collect_error(py, &msg),
            });
        }
    }
    Ok(())
}

/// of parameter sets. Marks appear in pytestmark order (bottom decorator
/// first); ids join in that order and later marks vary fastest.
pub(crate) fn expand_parametrize(
    py: Python<'_>,
    marks: &[MarkData],
    nodeid: &str,
    func: Option<&Bound<'_, PyAny>>,
    registry: &FixtureRegistry,
) -> PyResult<Vec<ParamVariant>> {
    let param_spec_cls = py.import("pytest")?.getattr("ParamSpec")?;
    let hidden_param = py.import("pytest")?.getattr("HIDDEN_PARAM")?;
    // Armed by configure_mark_generator once the session config is known.
    let strict_ids = py
        .import("pytest._marks")
        .and_then(|m| m.getattr("mark"))
        .and_then(|mark| mark.getattr("_strict_parametrization_ids"))
        .and_then(|v| v.extract::<bool>())
        .unwrap_or(false);
    // pytest_make_parametrize_id hook: conftest/plugin can override the ID
    // for each parameter value. Cached once; None when no config available.
    let config = crate::python::proxies::existing_py_config(py);
    let id_hook: Option<Bound<'_, PyAny>> = config.as_ref().and_then(|cfg| {
        cfg.bind(py)
            .getattr("hook")
            .ok()
            .and_then(|hook| hook.getattr("pytest_make_parametrize_id").ok())
    });
    let mut dims: Vec<Dim> = Vec::new();

    for mark in marks.iter().filter(|m| m.name == "parametrize") {
        let args = mark.obj.bind(py).getattr("args")?;
        let kwargs = mark.obj.bind(py).getattr("kwargs")?;
        // Both spellings are valid: positional or argnames=/argvalues= keywords.
        let argnames_obj = if args.len()? > 0 {
            args.get_item(0)?
        } else {
            kwargs.get_item("argnames")?
        };
        let argvalues = if args.len()? > 1 {
            args.get_item(1)?
        } else {
            kwargs.get_item("argvalues")?
        };
        // pytest's force_tuple: only a single argname given as a *string*
        // takes each argvalue as the bare value; a one-element list
        // (["x"]) still expects one-element value collections.
        let (argnames, force_scalar): (Vec<String>, bool) = match argnames_obj.extract::<String>() {
            Ok(joined) => {
                // Strip trailing empty names so that a trailing comma in the
                // argnames string (e.g. "a,b,c,") is silently ignored, matching
                // real pytest's behaviour.
                let mut names: Vec<String> =
                    joined.split(',').map(|s| s.trim().to_string()).collect();
                while names.last().map(|s: &String| s.is_empty()).unwrap_or(false) {
                    names.pop();
                }
                let single = names.len() == 1;
                (names, single)
            }
            Err(_) => (argnames_obj.extract()?, false),
        };
        let ids_obj = mark.obj.bind(py).getattr("kwargs")?.get_item("ids").ok();
        let n_argvalues = argvalues.len().unwrap_or(usize::MAX);
        let explicit_ids: Option<Vec<(Option<String>, bool)>> = ids_obj.as_ref().and_then(|ids| {
            if ids.is_callable() {
                return None;
            }
            let iter = ids.try_iter().ok()?;
            let mut result = Vec::new();
            for id in iter {
                if result.len() >= n_argvalues {
                    break;
                }
                let id = id.ok()?;
                if id.is(&hidden_param) {
                    result.push((None, true));
                } else if id.is_none() {
                    result.push((None, false));
                } else {
                    result.push((Some(id.extract::<String>().ok()?), false));
                }
            }
            Some(result)
        });
        // ids=callable: idfn(val) per value, None falling through to the
        // default id for that value (upstream _idval_from_function).
        let ids_callable = ids_obj.filter(|ids| ids.is_callable());
        // indirect=True routes every argname's value to the same-named
        // fixture's request.param; indirect=["x"] only the listed ones.
        let indirect_obj = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("indirect")
            .ok();
        let indirect_all = indirect_obj
            .as_ref()
            .and_then(|value| value.extract::<bool>().ok())
            .unwrap_or(false);
        let indirect_names: Vec<String> = indirect_obj
            .as_ref()
            .and_then(|value| {
                value
                    .extract::<Vec<String>>()
                    .ok()
                    .or_else(|| value.extract::<String>().ok().map(|s| vec![s]))
            })
            .unwrap_or_default();
        let is_indirect = |name: &str| indirect_all || indirect_names.iter().any(|n| n == name);
        let explicit_scope = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("scope")
            .ok()
            .and_then(|s| s.extract::<String>().ok())
            .and_then(|s| crate::fixture::Scope::parse(&s));
        let dim_scope = explicit_scope.unwrap_or_else(|| {
            if !indirect_all && indirect_names.is_empty() {
                return crate::fixture::Scope::Function;
            }
            let indirect_args: Vec<&str> = if indirect_all {
                argnames.iter().map(|s| s.as_str()).collect()
            } else {
                indirect_names.iter().map(|s| s.as_str()).collect()
            };
            indirect_args
                .iter()
                .filter_map(|name| registry.lookup(name, nodeid))
                .map(|def| def.scope)
                .min()
                .unwrap_or(crate::fixture::Scope::Function)
        });

        let mut sets = Vec::new();
        for (index, value_set) in argvalues.try_iter()?.enumerate() {
            let value_set = value_set?;
            let (values, spec_id, mut hidden, extra_marks) =
                if value_set.is_instance(&param_spec_cls)? {
                    let values: Vec<Bound<'_, PyAny>> = value_set
                        .getattr("values")?
                        .try_iter()?
                        .collect::<PyResult<_>>()?;
                    let id_obj = value_set.getattr("id")?;
                    let (spec_id, hidden) = if id_obj.is(&hidden_param) {
                        (None, true)
                    } else {
                        (id_obj.extract::<Option<String>>()?, false)
                    };
                    let extra_marks = value_set
                        .getattr("marks")?
                        .try_iter()?
                        .map(|m| {
                            let m = m?;
                            Ok(MarkData {
                                name: m.getattr("name")?.extract()?,
                                obj: m.unbind(),
                            })
                        })
                        .collect::<PyResult<Vec<_>>>()?;
                    (values, spec_id, hidden, extra_marks)
                } else if force_scalar {
                    (vec![value_set], None, false, Vec::new())
                } else {
                    let values: Vec<Bound<'_, PyAny>> =
                        value_set.try_iter()?.collect::<PyResult<_>>()?;
                    (values, None, false, Vec::new())
                };

            if values.len() != argnames.len() {
                // Upstream ParameterSet._for_parametrize wording, raised as
                // a bare CollectError (message only, no traceback).
                let names_repr = format!(
                    "[{}]",
                    argnames
                        .iter()
                        .map(|n| format!("'{n}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let values_repr: String = pyo3::types::PyTuple::new(py, values.iter())?
                    .repr()?
                    .extract()?;
                let message = format!(
                    "{nodeid}: in \"parametrize\" the number of names ({}):\n  {names_repr}\nmust be equal to the number of values ({}):\n  {values_repr}",
                    argnames.len(),
                    values.len()
                );
                return Err(collect_error(py, &message));
            }

            // Check explicit_ids for HIDDEN_PARAM at this index.
            if !hidden
                && let Some(ref ids) = explicit_ids
                && let Some((_, is_hidden)) = ids.get(index)
                && *is_hidden
            {
                hidden = true;
            }

            let id_part = if hidden {
                None
            } else {
                let from_callable = || -> PyResult<Option<String>> {
                    let Some(idfn) = ids_callable.as_ref() else {
                        return Ok(None);
                    };
                    let mut parts = Vec::new();
                    for (argname, value) in argnames.iter().zip(values.iter()) {
                        let id = idfn.call1((value,)).map_err(|err| {
                            collect_error(
                                py,
                                &format!(
                                    "{nodeid}: error raised while trying to determine id of \
                                     parameter '{argname}' at position {index}\n{}",
                                    err.value(py)
                                ),
                            )
                        })?;
                        parts.push(
                            user_id_from_value(py, &id)
                                .unwrap_or_else(|| id_for_value(value, argname, index)),
                        );
                    }
                    Ok(Some(parts.join("-")))
                };
                let callable_id = from_callable()?;
                Some(
                    spec_id
                        .or_else(|| {
                            explicit_ids
                                .as_ref()
                                .and_then(|ids| ids.get(index).and_then(|(s, _)| s.clone()))
                        })
                        .or(callable_id)
                        .unwrap_or_else(|| {
                            let parts: Vec<String> = argnames
                                .iter()
                                .zip(values.iter())
                                .map(|(argname, value)| {
                                    if let Some(ref hook) = id_hook {
                                        let kwargs = pyo3::types::PyDict::new(py);
                                        let _ = kwargs.set_item(
                                            "config",
                                            config.as_ref().map(|c| c.bind(py)),
                                        );
                                        let _ = kwargs.set_item("val", value);
                                        let _ = kwargs.set_item("argname", argname);
                                        if let Ok(result) = hook.call((), Some(&kwargs))
                                            && let Ok(s) = result.extract::<String>()
                                        {
                                            return s;
                                        }
                                    }
                                    id_for_value(value, argname, index)
                                })
                                .collect();
                            parts.join("-")
                        }),
                )
            };
            let mut params: Vec<(String, Py<PyAny>)> = Vec::new();
            let mut indirect_params: Vec<(String, usize, Py<PyAny>)> = Vec::new();
            for (argname, value) in argnames.iter().cloned().zip(values) {
                if is_indirect(&argname) {
                    indirect_params.push((argname, index, value.unbind()));
                } else {
                    params.push((argname, value.unbind()));
                }
            }
            sets.push(ParamSet {
                id_part,
                params,
                indirect_params,
                extra_marks,
            });
        }
        dedup_param_ids(py, &mut sets, nodeid, &argnames, strict_ids)?;
        if sets.is_empty() {
            sets.push(notset_param_set(
                py,
                &argnames,
                func,
                indirect_all,
                &indirect_names,
            )?);
        }
        dims.push(Dim {
            sets,
            scope: dim_scope,
        });
    }

    if dims.is_empty() {
        return Ok(vec![ParamVariant {
            id: None,
            params: Vec::new(),
            indirect_params: Vec::new(),
            extra_marks: Vec::new(),
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
        }]);
    }
    // An empty parameter set produces no items (pytest marks one skipped;
    // zero items is the closest simple behavior).
    if dims.iter().any(|dim| dim.sets.is_empty()) {
        return Ok(Vec::new());
    }

    Ok(cartesian_param_variants(py, &dims))
}

/// Cartesian product over the parametrize dimensions: the last dim varies
/// fastest and IDs join in dim order (matching stacked-decorator order).
fn cartesian_param_variants(py: Python<'_>, dims: &[Dim]) -> Vec<ParamVariant> {
    let mut variants = Vec::new();
    let mut indices = vec![0usize; dims.len()];
    'outer: loop {
        let mut id_parts = Vec::new();
        let mut params = Vec::new();
        let mut indirect_params = Vec::new();
        let mut extra_marks = Vec::new();
        for (dim, &index) in dims.iter().zip(indices.iter()) {
            let set = &dim.sets[index];
            // HIDDEN_PARAM sets contribute nothing to the test ID.
            if let Some(part) = &set.id_part {
                id_parts.push(part.clone());
            }
            for (name, value) in &set.params {
                params.push((name.clone(), value.clone_ref(py)));
            }
            for (name, param_index, value) in &set.indirect_params {
                indirect_params.push((name.clone(), *param_index, value.clone_ref(py)));
            }
            for mark in &set.extra_marks {
                extra_marks.push(MarkData {
                    name: mark.name.clone(),
                    obj: mark.obj.clone_ref(py),
                });
            }
        }
        let max_param_scope = dims
            .iter()
            .map(|d| d.scope)
            .max()
            .unwrap_or(crate::fixture::Scope::Function);
        let scope_sort_keys: Vec<(String, crate::fixture::Scope, usize)> = dims
            .iter()
            .zip(indices.iter())
            .filter(|(d, _)| d.scope > crate::fixture::Scope::Function)
            .map(|(d, &idx)| {
                let set = &d.sets[idx];
                let mut names: Vec<&str> = set.params.iter().map(|(n, _)| n.as_str()).collect();
                names.extend(set.indirect_params.iter().map(|(n, _, _)| n.as_str()));
                (names.join(","), d.scope, idx)
            })
            .collect();
        variants.push(ParamVariant {
            // All-hidden variants keep the bare test name (no brackets).
            id: (!id_parts.is_empty()).then(|| id_parts.join("-")),
            params,
            indirect_params,
            extra_marks,
            max_param_scope,
            scope_sort_keys,
        });

        for pos in (0..dims.len()).rev() {
            indices[pos] += 1;
            if indices[pos] < dims[pos].sets.len() {
                continue 'outer;
            }
            indices[pos] = 0;
            if pos == 0 {
                break 'outer;
            }
        }
    }
    variants
}

/// Resolve duplicate parameter-set IDs within one parametrize mark:
/// under strict_parametrization_ids a duplicate is a CollectError;
/// otherwise pytest's make_unique_parameterset_ids counter suffix applies.
fn dedup_param_ids(
    py: Python<'_>,
    sets: &mut [ParamSet],
    nodeid: &str,
    argnames: &[String],
    strict_ids: bool,
) -> PyResult<()> {
    let mut counts: std::collections::HashMap<Option<String>, usize> =
        std::collections::HashMap::new();
    for set in sets.iter() {
        *counts.entry(set.id_part.clone()).or_default() += 1;
    }
    if counts.values().any(|&count| count > 1) {
        let display = |id: &Option<String>| id.clone().unwrap_or_else(|| "<hidden>".to_string());
        if strict_ids {
            let mut reprs = Vec::new();
            for set in sets.iter() {
                let values = PyList::new(py, set.params.iter().map(|(_, value)| value.bind(py)))?;
                reprs.push(values.repr()?.to_string());
            }
            let mut seen = std::collections::HashSet::new();
            let duplicates: Vec<String> = sets
                .iter()
                .filter(|set| counts[&set.id_part] > 1)
                .filter(|set| seen.insert(set.id_part.clone()))
                .map(|set| display(&set.id_part))
                .collect();
            let ids: Vec<String> = sets.iter().map(|set| display(&set.id_part)).collect();
            let message = format!(
                "Duplicate parametrization IDs detected, but strict_parametrization_ids is set.\n\
                 \n\
                 Test name:      {nodeid}\n\
                 Parameters:     {}\n\
                 Parameter sets: {}\n\
                 IDs:            {}\n\
                 Duplicates:     {}\n\
                 \n\
                 You can fix this problem using `@pytest.mark.parametrize(..., ids=...)` or `pytest.param(..., id=...)`.",
                argnames.join(", "),
                reprs.join(", "),
                ids.join(", "),
                duplicates.join(", "),
            );
            return Err(collect_error(py, &message));
        }
        if counts.get(&None).copied().unwrap_or(0) > 1 {
            let func_name = nodeid.rsplit("::").next().unwrap_or(nodeid);
            let msg = format!(
                "In {func_name}: multiple instances of HIDDEN_PARAM cannot be used in \
                 the same parametrize call, because the tests names need to be unique."
            );
            let failed_result: PyResult<PyErr> = (|| {
                let cls = py.import("_pytest.outcomes")?.getattr("Failed")?;
                let instance = cls.call1((&msg,))?;
                instance.setattr("pytrace", false)?;
                Ok(PyErr::from_value(instance))
            })();
            return Err(match failed_result {
                Ok(err) => err,
                Err(_) => collect_error(py, &msg),
            });
        }
        let mut existing: std::collections::HashSet<String> =
            sets.iter().filter_map(|set| set.id_part.clone()).collect();
        let mut suffixes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for set in sets.iter_mut() {
            let Some(id) = set.id_part.clone() else {
                continue;
            };
            if counts[&Some(id.clone())] <= 1 {
                continue;
            }
            let sep = if id.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                "_"
            } else {
                ""
            };
            let counter = suffixes.entry(id.clone()).or_insert(0);
            let mut new_id = format!("{id}{sep}{counter}");
            while existing.contains(&new_id) {
                *counter += 1;
                new_id = format!("{id}{sep}{counter}");
            }
            existing.insert(new_id.clone());
            set.id_part = Some(new_id);
            *counter += 1;
        }
    }
    Ok(())
}

/// The single NOTSET parameter set pytest collects for empty argvalues,
/// carrying the configured empty_parameter_set_mark (default: skip).
fn notset_param_set(
    py: Python<'_>,
    argnames: &[String],
    func: Option<&Bound<'_, PyAny>>,
    indirect_all: bool,
    indirect_names: &[String],
) -> PyResult<ParamSet> {
    let mark_decorator = py
        .import("_pytest.mark")?
        .getattr("get_empty_parameterset_mark")?
        .call1((
            existing_py_config(py).unwrap_or_else(|| py.None()),
            argnames.to_vec(),
            func.map(|f| f.clone().unbind())
                .unwrap_or_else(|| py.None()),
        ))?;
    let mark_obj = mark_decorator.getattr("mark")?;
    let mark_name: String = mark_obj.getattr("name")?.extract()?;
    let notset = py.import("_pytest.compat")?.getattr("NOTSET")?;
    let mut params: Vec<(String, Py<PyAny>)> = Vec::new();
    let mut indirect_params: Vec<(String, usize, Py<PyAny>)> = Vec::new();
    for argname in argnames.iter().cloned() {
        if indirect_all || indirect_names.iter().any(|n| n == &argname) {
            indirect_params.push((argname, 0usize, notset.clone().unbind()));
        } else {
            params.push((argname, notset.clone().unbind()));
        }
    }
    Ok(ParamSet {
        id_part: Some("NOTSET".to_string()),
        params,
        indirect_params,
        extra_marks: vec![MarkData {
            name: mark_name,
            obj: mark_obj.unbind(),
        }],
    })
}
