use super::super::{network, ToolFuture, ToolLimits, ToolResponse, ToolServices, ToolSpec, TimeoutSetting};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicBool, Arc};
use tauri::AppHandle;

pub(in crate::assistant_tools) const SPEC: ToolSpec = ToolSpec {
    name: "web_search",
    description: "Search the web for current information. Do not invent a missing city.",
    argument: "query",
    max_argument_bytes: 4096,
    timeout: TimeoutSetting { default_secs: 8, min_secs: 1, max_secs: 300 },
    legacy_timeout: Some(|limits: &ToolLimits| limits.search_timeout_secs),
    execute,
};

fn execute<'a>(services: &'a ToolServices, argument: &'a str, cancel: Arc<AtomicBool>) -> ToolFuture<'a> {
    Box::pin(async move {
        let response = network::search(argument, services.brave_key.as_deref(), services.timeout(SPEC.name)?, &cancel).await?;
        Ok(result(services, response))
    })
}

fn result(services: &ToolServices, response: network::SearchResponse) -> ToolResponse {
    services.allow(response.results.iter().map(|result| result.url.clone()));
    let mut sources = Vec::new();
    let results = response.results.into_iter().map(|result| {
        let source = services.source(result.title, result.url);
        sources.push(source.clone());
        json!({"n":source.n,"title":source.title,"url":source.url,"snippet":result.snippet})
    }).collect::<Vec<_>>();
    ToolResponse::success(format!("Found {} search results.", results.len()), json!({"provider":response.provider,"results":results}), sources)
}

// Compatibility commands keep the existing settings format and transport, while this
// module owns search-specific validation and the only credential-bearing execution.
pub(crate) fn settings(key_set: bool) -> Value {
    json!({"provider":if key_set {"brave"} else {"duckduckgo"},"brave_key_set":key_set})
}
pub(crate) fn set_key(app: &AppHandle, provider: &str, key: Option<String>) -> Result<(), String> {
    let key = validated_key(provider, key)?;
    crate::settings::persist_assistant_search_key(app, key).map_err(|error|format!("{error:#}"))
}
fn validated_key(provider: &str, key: Option<String>) -> Result<Option<String>, String> {
    if provider != "brave" { return Err("Only Brave Search accepts a key.".into()); }
    let key = key.map(|key| key.trim().to_string()).filter(|key| !key.is_empty());
    if key.as_ref().is_some_and(|key|key.len() > 4096 || key.chars().any(char::is_control)) {
        return Err("Invalid search key.".into());
    }
    Ok(key)
}
pub(crate) async fn test_key(app: &AppHandle) -> Result<Value, String> {
    let (key, limits) = crate::settings::load_assistant_tool_settings(app);
    let key = key.ok_or("Save a Brave Search key first.")?;
    network::test_brave_key(&key, limits.timeout(SPEC.name)?, &AtomicBool::new(false)).await.map_err(|error|format!("{error:#}"))?;
    Ok(json!({"ok":true,"message":"Brave Search key works."}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_settings_validate_provider_and_key_without_exposing_credentials() {
        assert!(validated_key("other",None).is_err());
        assert_eq!(validated_key("brave",Some("  example  ".into())).unwrap(),Some("example".into()));
        assert_eq!(validated_key("brave",Some("   ".into())).unwrap(),None);
        let error = validated_key("brave",Some("secret\ncredential".into())).unwrap_err();
        assert_eq!(error,"Invalid search key.");
        assert_eq!(settings(false)["provider"],"duckduckgo");
        assert_eq!(settings(true)["provider"],"brave");
        assert!(settings(true).get("key").is_none());
    }
    #[test]
    fn search_results_grant_exact_urls_and_reuse_source_numbers() {
        let services = crate::assistant_tools::tests::services("search-handler-result");
        let response = || network::SearchResponse { provider:"duckduckgo".into(), results:vec![network::SearchResult {
            title:"A result".into(), url:"https://example.com/page".into(), snippet:"A summary".into(),
        }] };
        let first = result(&services, response());
        let second = result(&services, response());
        assert_eq!(first.sources[0].n, second.sources[0].n);
        assert_eq!(first.data["results"][0]["snippet"], "A summary");
        assert!(services.allowed_urls().unwrap().contains("https://example.com/page"));
        assert!(!services.allowed_urls().unwrap().contains("https://example.com/other"));
    }
}
