//! Add a tool module and one line here. Registration drives schemas, validation,
//! dispatch, selection, and timeout defaults; handlers remain ordinary Rust functions.
use super::ToolSpec;

macro_rules! available_tools {
    ($($module:ident => $path:literal),+ $(,)?) => {
        $(#[path = $path] pub(super) mod $module;)+
        pub(super) const ALL: &[ToolSpec] = &[$($module::SPEC),+];
    };
}

available_tools! {
    web_search => "web_search.rs",
    fetch_page => "fetch_page.rs",
    get_weather => "get_weather.rs",
    run_python => "run_python.rs",
}
