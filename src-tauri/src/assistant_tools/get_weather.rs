use super::super::{network, ToolFuture, ToolLimits, ToolResponse, ToolServices, ToolSpec, TimeoutSetting};
use serde_json::json;
use std::sync::{atomic::AtomicBool, Arc};

pub(in crate::assistant_tools) const SPEC: ToolSpec = ToolSpec {
    name: "get_weather",
    description: "Get current weather and today's forecast in Celsius. Ask the user for a city if absent.",
    argument: "city",
    max_argument_bytes: 4096,
    timeout: TimeoutSetting { default_secs: 5, min_secs: 1, max_secs: 300 },
    legacy_timeout: Some(|limits: &ToolLimits| limits.weather_timeout_secs),
    execute,
};
fn execute<'a>(services: &'a ToolServices, argument: &'a str, cancel: Arc<AtomicBool>) -> ToolFuture<'a> {
    Box::pin(async move {
        let weather = network::get_weather(argument, services.timeout(SPEC.name)?, &cancel).await?;
        Ok(result(services, argument, weather))
    })
}
fn result(services: &ToolServices, city: &str, weather: network::WeatherResult) -> ToolResponse {
    let source = services.source("Open-Meteo (CC BY 4.0)".into(), weather.url);
    ToolResponse::success(format!("Weather for {city}."), json!({"source":source,"weather":weather.text}), vec![source])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weather_result_keeps_attribution_and_provider_text() {
        let services = crate::assistant_tools::tests::services("weather-handler-result");
        let result = result(&services,"Singapore",network::WeatherResult {text:"Singapore, 29 °C, Asia/Singapore".into(),url:"https://api.open-meteo.com/forecast".into()});
        assert_eq!(result.summary,"Weather for Singapore.");
        assert_eq!(result.sources[0].title,"Open-Meteo (CC BY 4.0)");
        assert_eq!(result.data["weather"],"Singapore, 29 °C, Asia/Singapore");
    }
}
