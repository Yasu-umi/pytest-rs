//! Python-side collection: modules, classes, TestCases, doctests, parametrize.

mod doctest;
mod hooks;
mod introspect;
mod items;
mod parametrize;
mod utils;

pub use introspect::AsyncFlags;
pub(crate) use introspect::NameFilters;
pub use introspect::async_flags;
pub use introspect::num_mock_patch_args;
pub use introspect::param_names;
pub use introspect::param_names_with_positional_only;
pub use introspect::pyargs_anchor;
pub use introspect::resolve_pyarg;

pub use hooks::CollectDirResult;
pub use hooks::CustomCollectResult;
pub use hooks::call_collect_directory_hook;
pub use hooks::call_pycollect_makemodule_hook;
pub use hooks::collect_custom_files;
pub use hooks::has_collect_directory_hook;
pub use hooks::has_collect_file_hook;
pub use hooks::has_pycollect_makeitem_hook;
pub use hooks::has_pycollect_makemodule_hook;
pub use hooks::walk_collect_directories;

pub use doctest::collect_doctests_from_module;
pub use doctest::collect_doctests_from_textfile;
pub use doctest::collect_module;
pub use doctest::is_doctest_textfile;

pub(crate) use utils::collect_error;
pub use utils::collect_error_message;
pub(crate) use utils::fixture_param_id;
pub use utils::glob_testpaths;
pub(crate) use utils::id_for_value;
pub use utils::keyword_match_names;
