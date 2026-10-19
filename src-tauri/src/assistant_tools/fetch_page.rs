use super::super::{network, ToolFuture, ToolLimits, ToolResponse, ToolServices, ToolSpec, TimeoutSetting};
use serde_json::json;
use std::sync::{atomic::AtomicBool, Arc};

pub(in crate::assistant_tools) const SPEC: ToolSpec = ToolSpec {
    name: "fetch_page",
    description: "Read a URL supplied by the user or returned by web search.",
    argument: "url",
    max_argument_bytes: 4096,
    timeout: TimeoutSetting { default_secs: 10, min_secs: 1, max_secs: 300 },
    legacy_timeout: Some(|limits: &ToolLimits| limits.fetch_timeout_secs),
    execute,
};

fn execute<'a>(services: &'a ToolServices, argument: &'a str, cancel: Arc<AtomicBool>) -> ToolFuture<'a> {
    Box::pin(async move {
        let allowed = services.allowed_urls()?;
        let page = network::fetch_page(argument, &allowed, services.timeout(SPEC.name)?, &cancel).await?;
        Ok(result(services, page))
    })
}
fn result(services: &ToolServices, page: network::PageResult) -> ToolResponse {
    let source = services.source(page.title.clone(), page.url.clone());
    ToolResponse::success(format!("Opened {}{}.", page.title, if page.truncated { " (truncated)" } else { "" }),
        json!({"source":source,"text":page.text,"truncated":page.truncated}), vec![source])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn missing_url_grant_fails_before_network() {
        let services = crate::assistant_tools::tests::services("fetch-handler-denied");
        let error = execute(&services, "https://example.com/unlisted", Arc::new(AtomicBool::new(false))).await.err().unwrap();
        assert!(error.to_string().contains("exact URL"));
    }
    #[test]
    fn page_result_records_final_redirect_url_and_truncation() {
        let services = crate::assistant_tools::tests::services("fetch-handler-result");
        let result = result(&services, network::PageResult {title:"Example".into(),url:"https://example.com/final".into(),text:"Readable text".into(),truncated:true});
        assert_eq!(result.sources[0].url,"https://example.com/final");
        assert_eq!(result.data["text"],"Readable text");
        assert!(result.summary.contains("truncated"));
    }
}
