//! Network-only Assistant tools. The dispatcher owns conversation allowlists and source numbering.
//! No credential-bearing request follows redirects; fetched URLs are DNS-vetted and pinned per hop.
use anyhow::{anyhow, bail, Result};
use reqwest::{header, Client, Response, Url};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const SEARCH_CAP: usize = 1_000_000;
const WEATHER_CAP: usize = 256_000;
const PAGE_CAP: usize = 2_000_000;
const PAGE_CHARS: usize = 24_000;
const SEARCH_UNAVAILABLE: &str =
    "Search is unavailable right now. You can add a Brave Search key in Settings.";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub provider: String,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct PageResult {
    pub title: String,
    pub url: String,
    pub text: String,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WeatherResult {
    pub text: String,
    pub url: String,
}

async fn bounded<T>(
    cancel: &AtomicBool,
    timeout: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    if cancel.load(Ordering::SeqCst) {
        bail!("Tool cancelled.");
    }
    let cancellation = async {
        loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if cancel.load(Ordering::SeqCst) {
                break;
            }
        }
    };
    tokio::select! {
        biased;
        _ = cancellation => Err(anyhow!("Tool cancelled.")),
        result = tokio::time::timeout(timeout, work) => result.map_err(|_| anyhow!("Tool timed out."))?,
    }
}

fn client() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!("VoiceReader/", env!("CARGO_PKG_VERSION")))
}

/// URL-library normalization only: never decode escaped path separators or collapse a query.
/// Fragments are not transmitted over HTTP and therefore do not change fetch authorization.
pub(crate) fn normalize_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw).map_err(|_| anyhow!("Invalid page URL."))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("Only public HTTP and HTTPS pages can be opened.");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs containing credentials cannot be opened.");
    }
    url.set_fragment(None);
    Ok(url.into())
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if s[..5] == [0; 5] && matches!(s[5], 0 | 0xffff) {
                IpAddr::V4(Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                ))
            } else {
                ip
            }
        }
        _ => ip,
    }
}

fn public_ip(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Public global unicast only; refuse special-use and IPv4 transition mechanisms.
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

fn vetted_addresses(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>> {
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        bail!("This page resolves to a non-public network address and cannot be opened.");
    }
    Ok(addresses)
}

async fn guarded_client(url: &Url) -> Result<Client> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("Invalid page host."))?;
    let literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("Invalid page port."))?;
    let addresses = match literal {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| anyhow!("Could not resolve the page host."))?
            .collect(),
    };
    let addresses = vetted_addresses(addresses)?;
    // Retain URL hostname for Host/TLS/SNI. Only the socket destination is overridden.
    client()
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| anyhow!("Could not create the page client."))
}

async fn bytes(mut response: Response, cap: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .map_or(false, |length| length > cap as u64)
    {
        bail!("The response exceeds the tool's download limit.");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("The download stopped unexpectedly."))?
    {
        if chunk.len() > cap.saturating_sub(body.len()) {
            bail!("The response exceeds the tool's download limit.");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn status_error(status: reqwest::StatusCode) -> anyhow::Error {
    match status.as_u16() {
        401 | 403 => {
            anyhow!("The provider refused access. Check your search key or try again later.")
        }
        429 => anyhow!("The provider's rate limit was reached. Try again later."),
        _ => anyhow!("The provider returned HTTP {}.", status.as_u16()),
    }
}

/// A web page's HTTP error, worded for a page rather than a search provider.
fn page_status_error(status: reqwest::StatusCode) -> anyhow::Error {
    match status.as_u16() {
        401 | 403 => anyhow!("The site refused access to this page (HTTP {}).", status.as_u16()),
        404 | 410 => anyhow!("The page was not found (HTTP {}).", status.as_u16()),
        429 => anyhow!("The site is limiting requests. Try again later."),
        code => anyhow!("The site returned HTTP {code}."),
    }
}

async fn provider_get(url: Url, key: Option<&str>, cap: usize) -> Result<Vec<u8>> {
    let http = client()
        .build()
        .map_err(|_| anyhow!("Could not create the provider client."))?;
    let mut request = http.get(url);
    if let Some(key) = key {
        let mut value = header::HeaderValue::from_str(key)
            .map_err(|_| anyhow!("The search key contains invalid characters."))?;
        value.set_sensitive(true);
        request = request.header("X-Subscription-Token", value);
    }
    // Deliberately discard reqwest errors: URLs and credential-bearing headers must not leak.
    let response = request
        .send()
        .await
        .map_err(|_| anyhow!("Could not reach the provider."))?;
    if !response.status().is_success() {
        return Err(status_error(response.status()));
    }
    bytes(response, cap).await
}

fn clipped(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("static selector")
}
fn plain_html(fragment: &str) -> String {
    clean(
        &Html::parse_fragment(fragment)
            .root_element()
            .text()
            .collect::<String>(),
    )
}

fn ddg_target(href: &str) -> Option<String> {
    let base = Url::parse("https://html.duckduckgo.com/html/").ok()?;
    let link = base.join(href).ok()?;
    let host = link.host_str()?;
    let raw = if matches!(
        host,
        "duckduckgo.com" | "www.duckduckgo.com" | "html.duckduckgo.com"
    ) && link.path() == "/l/"
    {
        link.query_pairs()
            .find(|(key, _)| key == "uddg")?
            .1
            .into_owned()
    } else {
        link.to_string()
    };
    normalize_url(&raw).ok()
}

/// A block or CAPTCHA page has no result elements, so it is reported by the empty-results
/// check below. Matching words like "captcha" in the body would also reject a normal results
/// page that merely mentions them ("how to solve a captcha").
fn parse_ddg(body: &str) -> Result<Vec<SearchResult>> {
    let doc = Html::parse_document(body);
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for result in doc.select(&selector(".result")) {
        // Sponsored results link through DuckDuckGo's ad redirect, not to the page itself.
        if result.value().classes().any(|class| class == "result--ad") {
            continue;
        }
        let Some(anchor) = result.select(&selector("a.result__a")).next() else {
            continue;
        };
        let Some(url) = anchor.value().attr("href").and_then(ddg_target) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        results.push(SearchResult {
            title: clipped(&clean(&anchor.text().collect::<String>()), 160),
            url,
            snippet: result
                .select(&selector(".result__snippet"))
                .next()
                .map(|e| clipped(&clean(&e.text().collect::<String>()), 280))
                .unwrap_or_default(),
        });
        if results.len() == 5 {
            break;
        }
    }
    if results.is_empty()
        && doc
            .select(&selector(".no-results, .no-results__message"))
            .next()
            .is_none()
    {
        bail!(SEARCH_UNAVAILABLE);
    }
    Ok(results)
}

async fn throttle_ddg() {
    static NEXT: OnceLock<Mutex<Instant>> = OnceLock::new();
    loop {
        // Admit at most one caller per interval. Cancelled waiters reserve no future slots.
        let wait = {
            let mut next = NEXT
                .get_or_init(|| Mutex::new(Instant::now()))
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            if now >= *next {
                *next = now + Duration::from_secs(2);
                return;
            }
            next.saturating_duration_since(now)
        };
        tokio::time::sleep(wait).await;
    }
}

fn parse_brave(body: &[u8]) -> Result<Vec<SearchResult>> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|_| anyhow!("The search provider sent an unreadable response."))?;
    let mut results = Vec::new();
    if let Some(items) = data.pointer("/web/results").and_then(Value::as_array) {
        for item in items {
            let Some(url) = item
                .get("url")
                .and_then(Value::as_str)
                .and_then(|u| normalize_url(u).ok())
            else {
                continue;
            };
            results.push(SearchResult {
                title: clipped(&plain_html(item["title"].as_str().unwrap_or("")), 160),
                url,
                snippet: clipped(&plain_html(item["description"].as_str().unwrap_or("")), 280),
            });
            if results.len() == 5 {
                break;
            }
        }
    } else if data.get("web").is_some() || data.get("query").is_none() {
        bail!("The search provider sent an unreadable response.");
    }
    Ok(results)
}

async fn brave_search(query: &str, key: &str) -> Result<Vec<SearchResult>> {
    let mut url = Url::parse("https://api.search.brave.com/res/v1/web/search")?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("count", "5");
    parse_brave(&provider_get(url, Some(key), SEARCH_CAP).await?)
}

pub(crate) async fn search(
    query: &str,
    brave_key: Option<&str>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<SearchResponse> {
    bounded(cancel, timeout, async {
        if query.trim().is_empty() || query.chars().count() > 2000 {
            bail!("Enter a search query of 1–2,000 characters.");
        }
        if let Some(key) = brave_key.filter(|key| !key.trim().is_empty()) {
            Ok(SearchResponse {
                results: brave_search(query, key).await?,
                provider: "brave".into(),
            })
        } else {
            throttle_ddg().await;
            let mut url = Url::parse("https://html.duckduckgo.com/html/")?;
            url.query_pairs_mut().append_pair("q", query);
            let body = provider_get(url, None, SEARCH_CAP)
                .await
                .map_err(|_| anyhow!(SEARCH_UNAVAILABLE))?;
            Ok(SearchResponse {
                results: parse_ddg(&String::from_utf8_lossy(&body))?,
                provider: "duckduckgo".into(),
            })
        }
    })
    .await
}

pub(crate) async fn test_brave_key(
    key: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    bounded(cancel, timeout, async {
        if key.trim().is_empty() {
            bail!("Enter a Brave Search key first.");
        }
        brave_search("weather", key).await?;
        Ok(())
    })
    .await
}

fn redirect_url(current: &Url, location: &str) -> Result<Url> {
    let next = current
        .join(location)
        .map_err(|_| anyhow!("The page returned an invalid redirect."))?;
    let next = Url::parse(&normalize_url(next.as_str())?)?;
    if current.scheme() == "https" && next.scheme() != "https" {
        bail!("The page tried to redirect from HTTPS to HTTP.");
    }
    Ok(next)
}

fn html_text(element: ElementRef<'_>, output: &mut String) {
    html_text_at_depth(element, output, 0)
}

fn html_text_at_depth(element: ElementRef<'_>, output: &mut String, depth: usize) {
    // Remote HTML is untrusted; nesting must not exhaust the native stack.
    if depth > 128 {
        output.push_str("\n[Deeply nested content omitted.]\n");
        return;
    }
    let name = element.value().name();
    if matches!(
        name,
        "script" | "style" | "nav" | "header" | "footer" | "noscript" | "template" | "svg" | "form"
    ) || element.value().attr("hidden").is_some()
        || element.value().attr("aria-hidden") == Some("true")
        || element.value().attr("role").map_or(false, |r| {
            matches!(r, "navigation" | "banner" | "contentinfo")
        })
    {
        return;
    }
    let block = matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "tr"
            | "br"
            | "pre"
            | "blockquote"
    );
    if block {
        output.push('\n');
    }
    for child in element.children() {
        if let Some(text) = child.value().as_text() {
            output.push_str(text);
        } else if let Some(element) = ElementRef::wrap(child) {
            html_text_at_depth(element, output, depth + 1);
        }
    }
    if matches!(name, "td" | "th") {
        output.push_str(" | ");
    }
    if block {
        output.push('\n');
    }
}

fn extract_page(body: &[u8], mime: &str, url: &str) -> Result<PageResult> {
    let raw = String::from_utf8_lossy(body);
    let (title, text) = if matches!(mime, "text/html" | "application/xhtml+xml") {
        let doc = Html::parse_document(&raw);
        let title = doc
            .select(&selector("title"))
            .next()
            .map(|e| clean(&e.text().collect::<String>()))
            .unwrap_or_default();
        let root = doc
            .select(&selector("main, article"))
            .next()
            .unwrap_or_else(|| doc.root_element());
        let mut text = String::new();
        html_text(root, &mut text);
        (
            title,
            text.lines()
                .map(clean)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        (String::new(), raw.into_owned())
    };
    let truncated = text.chars().count() > PAGE_CHARS;
    let mut text = clipped(&text, PAGE_CHARS);
    if truncated {
        text.push_str("\n[Page text truncated to approximately 6,000 tokens.]");
    }
    Ok(PageResult {
        title: clipped(&title, 300),
        url: url.into(),
        text,
        truncated,
    })
}

pub(crate) async fn fetch_page(
    raw: &str,
    allowed: &HashSet<String>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<PageResult> {
    bounded(cancel, timeout, async {
        let normalized = normalize_url(raw)?;
        if !allowed.contains(&normalized) {
            bail!("This exact URL has not appeared in the conversation or its search results.");
        }
        let mut url = Url::parse(&normalized)?;
        for hop in 0..=5 {
            let http = guarded_client(&url).await?;
            let response = http
                .get(url.clone())
                .send()
                .await
                .map_err(|_| anyhow!("Could not open the page."))?;
            if response.status().is_redirection() {
                if hop == 5 {
                    bail!("The page redirected too many times.");
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow!("The page returned an invalid redirect."))?;
                url = redirect_url(&url, location)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(page_status_error(response.status()));
            }
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !matches!(
                mime.as_str(),
                "text/html"
                    | "application/xhtml+xml"
                    | "text/plain"
                    | "text/markdown"
                    | "text/x-markdown"
            ) {
                bail!("Only HTML, plain text and Markdown pages can be opened.");
            }
            let body = bytes(response, PAGE_CAP).await?;
            return extract_page(&body, &mime, url.as_str());
        }
        unreachable!()
    })
    .await
}

async fn provider_json(url: Url) -> Result<Value> {
    serde_json::from_slice(&provider_get(url, None, WEATHER_CAP).await?)
        .map_err(|_| anyhow!("The weather provider sent an unreadable response."))
}

fn weather_text(place: &Value, weather: &Value) -> Result<String> {
    let name = place["name"]
        .as_str()
        .ok_or_else(|| anyhow!("The weather location was unreadable."))?;
    let country = place["country"].as_str().unwrap_or("");
    let region = place["admin1"].as_str().unwrap_or("");
    let zone = weather["timezone"]
        .as_str()
        .ok_or_else(|| anyhow!("The weather time zone was unavailable."))?;
    let value = |path| weather.pointer(path).and_then(Value::as_f64).map(|n| n.to_string());
    let current = value("/current/temperature_2m")
        .ok_or_else(|| anyhow!("The weather provider did not return the current temperature."))?;
    let code = weather
        .pointer("/current/weather_code")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("Weather conditions were unavailable."))?;
    let condition = match code {
        0 => "clear",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "fog",
        51..=57 => "drizzle",
        61..=67 => "rain",
        71..=77 => "snow",
        80..=82 => "rain showers",
        85 | 86 => "snow showers",
        95..=99 => "thunderstorm",
        _ => "unknown conditions",
    };
    let place = [name, region, country]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    // Open-Meteo returns null for some daily values in some places; report what it has.
    let missing = || "not available".to_string();
    let high = value("/daily/temperature_2m_max/0").map(|n| format!("{n} °C")).unwrap_or_else(missing);
    let low = value("/daily/temperature_2m_min/0").map(|n| format!("{n} °C")).unwrap_or_else(missing);
    let rain = value("/daily/precipitation_probability_max/0").map(|n| format!("{n}%")).unwrap_or_else(missing);
    Ok(format!("{place}. Time zone: {zone}. Current: {current} °C, {condition}. Today's high: {high}; low: {low}; precipitation chance: {rain}. Source: Open-Meteo (CC BY 4.0), https://open-meteo.com/."))
}

/// Open-Meteo's geocoder matches a place name only, so "Paris, France" finds nothing. Search
/// for the part before the first comma and use the rest to pick among the matches.
fn geocode_query(city: &str) -> (&str, Option<&str>) {
    match city.split_once(',') {
        Some((name, rest)) if !name.trim().is_empty() && !rest.trim().is_empty() => (name.trim(), Some(rest.trim())),
        _ => (city.trim(), None),
    }
}

/// The first match whose country, country code or region contains every word of the
/// qualifier ("France", "FR", "Texas, US"), else the first match.
fn pick_place<'a>(results: &'a [Value], qualifier: Option<&str>) -> Option<&'a Value> {
    let wanted: Vec<String> = qualifier
        .map(|q| q.split([',', ' ']).filter(|w| !w.is_empty()).map(str::to_lowercase).collect())
        .unwrap_or_default();
    let fields = |place: &Value| {
        ["country", "country_code", "admin1", "admin2"]
            .iter()
            .filter_map(|key| place[*key].as_str())
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    results
        .iter()
        .find(|place| !wanted.is_empty() && wanted.iter().all(|word| fields(place).iter().any(|field| field == word || field.split(' ').any(|part| part == word))))
        .or_else(|| results.first())
}

pub(crate) async fn get_weather(
    city: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<WeatherResult> {
    bounded(cancel, timeout, async {
        if city.trim().is_empty() || city.chars().count() > 300 {
            bail!("Specify a city of 1–300 characters.");
        }
        let (name, qualifier) = geocode_query(city);
        let mut geo = Url::parse("https://geocoding-api.open-meteo.com/v1/search")?;
        geo.query_pairs_mut()
            .append_pair("name", name)
            .append_pair("count", if qualifier.is_some() { "10" } else { "1" })
            .append_pair("language", "en")
            .append_pair("format", "json");
        let locations = provider_json(geo).await?;
        let results = locations["results"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let place = pick_place(results, qualifier).ok_or_else(|| {
            anyhow!("No matching city was found. Please specify the city and country.")
        })?;
        let lat = place["latitude"]
            .as_f64()
            .ok_or_else(|| anyhow!("The city's coordinates were unavailable."))?;
        let lon = place["longitude"]
            .as_f64()
            .ok_or_else(|| anyhow!("The city's coordinates were unavailable."))?;
        let mut url = Url::parse("https://api.open-meteo.com/v1/forecast")?;
        url.query_pairs_mut()
            .append_pair("latitude", &lat.to_string())
            .append_pair("longitude", &lon.to_string())
            .append_pair("current", "temperature_2m,weather_code")
            .append_pair(
                "daily",
                "temperature_2m_max,temperature_2m_min,precipitation_probability_max",
            )
            .append_pair("timezone", "auto")
            .append_pair("forecast_days", "1")
            .append_pair("temperature_unit", "celsius");
        let weather = provider_json(url.clone()).await?;
        Ok(WeatherResult {
            text: weather_text(place, &weather)?,
            url: url.into(),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn page_errors_never_mention_a_search_key() {
        for code in [401u16, 403, 404, 429, 500] {
            let message = page_status_error(reqwest::StatusCode::from_u16(code).unwrap()).to_string();
            assert!(!message.to_lowercase().contains("key"), "{message}");
        }
        assert!(page_status_error(reqwest::StatusCode::FORBIDDEN).to_string().contains("refused access to this page"));
    }
    #[test]
    fn city_and_country_names_are_split_and_matched() {
        assert_eq!(geocode_query("Paris, France"), ("Paris", Some("France")));
        assert_eq!(geocode_query("Singapore"), ("Singapore", None));
        assert_eq!(geocode_query(" , x"), (", x", None));
        let results = [
            json!({"name":"Paris","country":"United States","country_code":"US","admin1":"Texas"}),
            json!({"name":"Paris","country":"France","country_code":"FR","admin1":"Île-de-France"}),
        ];
        assert_eq!(pick_place(&results, Some("France")).unwrap()["country_code"], "FR");
        assert_eq!(pick_place(&results, Some("fr")).unwrap()["country_code"], "FR");
        assert_eq!(pick_place(&results, Some("Texas, US")).unwrap()["country_code"], "US");
        assert_eq!(pick_place(&results, Some("Atlantis")).unwrap()["country_code"], "US", "no match falls back to the first");
        assert_eq!(pick_place(&results, None).unwrap()["country_code"], "US");
        assert!(pick_place(&[], Some("France")).is_none());
    }
    #[test]
    fn exact_normalization_preserves_escaped_path_and_query() {
        assert_eq!(
            normalize_url("https://EXAMPLE.com:443/a#part").unwrap(),
            "https://example.com/a"
        );
        assert_ne!(
            normalize_url("https://example.com/a%2Fb").unwrap(),
            normalize_url("https://example.com/a/b").unwrap()
        );
        assert_ne!(
            normalize_url("https://example.com/?a=1&b=2").unwrap(),
            normalize_url("https://example.com/?b=2&a=1").unwrap()
        );
        for bad in [
            "file:///etc/passwd",
            "http://user:secret@example.com",
            "https://example.com@127.0.0.1:8080",
        ] {
            assert!(normalize_url(bad).is_err());
        }
    }
    #[test]
    fn refuses_nonpublic_and_ipv4_embedded_addresses() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::192.168.1.1",
            "64:ff9b::a00:1",
            "2002:7f00:1::1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
    #[test]
    fn mixed_dns_fails_closed() {
        assert!(vetted_addresses(vec![
            "8.8.8.8:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap()
        ])
        .is_err());
        assert!(vetted_addresses(vec![]).is_err());
        assert_eq!(
            vetted_addresses(vec!["8.8.8.8:443".parse().unwrap()])
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn redirects_allow_relative_and_public_origin_change_but_never_downgrade() {
        let base = Url::parse("https://example.com/a/b").unwrap();
        assert_eq!(
            redirect_url(&base, "../c").unwrap().as_str(),
            "https://example.com/c"
        );
        assert_eq!(
            redirect_url(&base, "https://example.org:8443/x")
                .unwrap()
                .port(),
            Some(8443)
        );
        assert!(redirect_url(&base, "http://example.com/x").is_err());
        assert!(redirect_url(&base, "https://user:password@example.org/x").is_err());
        let private = redirect_url(&base, "http://127.0.0.1/x");
        assert!(private.is_err());
        let private = redirect_url(&base, "https://127.0.0.1/x").unwrap();
        assert!(!public_ip(private.host_str().unwrap().parse().unwrap()));
    }
    #[test]
    fn parses_ddg_fixture_decodes_redirect_and_skips_invalid_duplicate_links() {
        let results = parse_ddg(include_str!("../tests-data/tools/ddg-results.html")).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://example.com/science?x=1");
        assert_eq!(results[0].title, "Science & research");
        assert_eq!(results[0].snippet, "A useful result about science.");
    }
    #[test]
    fn ddg_results_that_mention_captcha_still_parse_and_ads_are_skipped() {
        let page = r#"<div class="result result--ad"><a class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x">Ad</a></div>
<div class="result"><a class="result__a" href="https://example.com/captcha-guide">How to solve a captcha</a><div class="result__snippet">Why sites show a CAPTCHA challenge-form.</div></div>"#;
        let results = parse_ddg(page).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/captcha-guide");
    }
    #[test]
    fn ddg_captcha_and_changed_page_report_actionable_error() {
        for page in [
            include_str!("../tests-data/tools/ddg-captcha.html"),
            "<html>unexpected structure</html>",
        ] {
            assert_eq!(parse_ddg(page).unwrap_err().to_string(), SEARCH_UNAVAILABLE);
        }
        assert!(parse_ddg("<div class='no-results'>No results found</div>")
            .unwrap()
            .is_empty());
    }
    #[test]
    fn brave_fixture_strips_markup_and_rejects_unsafe_schemes() {
        let results =
            parse_brave(include_bytes!("../tests-data/tools/brave-results.json")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Science & research");
        assert_eq!(results[0].snippet, "A useful result.");
        assert!(parse_brave(b"{}").is_err());
    }
    #[test]
    fn extraction_keeps_structure_and_discards_noncontent() {
        let page = extract_page(
            include_bytes!("../tests-data/tools/page.html"),
            "text/html",
            "https://example.com/",
        )
        .unwrap();
        assert_eq!(page.title, "Example & analysis");
        assert!(page
            .text
            .contains("Research report\nFirst important finding."));
        assert!(page.text.contains("Year | Value |"));
        assert!(page.text.contains("2026 | 42 |"));
        assert!(!page.text.contains("SECRET"));
        assert!(!page.truncated);
        let large = "é".repeat(PAGE_CHARS + 1);
        let page = extract_page(large.as_bytes(), "text/plain", "https://example.com").unwrap();
        assert!(page.truncated);
        assert!(page
            .text
            .ends_with("[Page text truncated to approximately 6,000 tokens.]"));
    }
    #[test]
    fn weather_names_found_city_zone_units_conditions_and_attribution() {
        let data: Value =
            serde_json::from_str(include_str!("../tests-data/tools/weather.json")).unwrap();
        let text = weather_text(&data["place"], &data["weather"]).unwrap();
        for value in [
            "Singapore",
            "Asia/Singapore",
            "29.5 °C",
            "overcast",
            "32.1 °C",
            "25.4 °C",
            "75%",
            "Open-Meteo (CC BY 4.0)",
        ] {
            assert!(text.contains(value), "{value}");
        }
        assert!(weather_text(&data["place"], &serde_json::json!({})).is_err());
        let mut partial = data["weather"].clone();
        partial["daily"]["precipitation_probability_max"] = json!([null]);
        let text = weather_text(&data["place"], &partial).unwrap();
        assert!(text.contains("precipitation chance: not available"), "{text}");
        assert!(text.contains("Today's high: 32.1 °C"), "{text}");
    }
    #[tokio::test]
    async fn cancellation_and_deadline_drop_inflight_work() {
        let cancel = AtomicBool::new(true);
        assert!(bounded(&cancel, Duration::from_secs(1), async { Ok(()) })
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        let cancel = AtomicBool::new(false);
        let work = std::future::pending::<Result<()>>();
        assert!(bounded(&cancel, Duration::from_millis(10), work)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        let work = async {
            cancel.store(true, Ordering::SeqCst);
            std::future::pending::<Result<()>>().await
        };
        assert!(bounded(&cancel, Duration::from_secs(1), work)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }
    #[tokio::test]
    async fn fetch_requires_exact_allowlist_before_network_and_blocks_local_redirect_destination() {
        let cancel = AtomicBool::new(false);
        let allowed = HashSet::from(["https://example.com/a".to_owned()]);
        assert!(fetch_page(
            "https://example.com/b",
            &allowed,
            Duration::from_secs(1),
            &cancel
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("exact URL"));
        let allowed = HashSet::from(["http://127.0.0.1/".to_owned()]);
        assert!(fetch_page(
            "http://127.0.0.1/",
            &allowed,
            Duration::from_secs(1),
            &cancel
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("non-public"));
        assert!(
            guarded_client(&Url::parse("https://[::ffff:127.0.0.1]/").unwrap())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn capped_body_rejects_claimed_and_streamed_overflow() {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n0\r\n\r\n",
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096]; let _ = stream.read(&mut request);
                stream.write_all(response.as_bytes()).unwrap();
            });
            let response = client().build().unwrap().get(format!("http://{addr}")).send().await.unwrap();
            assert!(bytes(response, 5).await.unwrap_err().to_string().contains("download limit"));
        }
    }
    #[test]
    fn provider_status_errors_are_credential_free_and_actionable() {
        assert!(status_error(reqwest::StatusCode::UNAUTHORIZED)
            .to_string()
            .contains("search key"));
        assert!(status_error(reqwest::StatusCode::TOO_MANY_REQUESTS)
            .to_string()
            .contains("rate limit"));
        assert_eq!(
            status_error(reqwest::StatusCode::FOUND).to_string(),
            "The provider returned HTTP 302."
        );
    }
    #[tokio::test]
    async fn ddg_throttle_serializes_search_admission() {
        throttle_ddg().await;
        let started = Instant::now();
        throttle_ddg().await;
        assert!(started.elapsed() >= Duration::from_secs(2));
    }
    #[tokio::test]
    async fn provider_redirect_never_forwards_key() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let error = provider_get(
            Url::parse(&format!("http://{addr}")).unwrap(),
            Some("test-key-never-forward"),
            100,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "The provider returned HTTP 302.");
        assert!(!error.to_string().contains("test-key-never-forward"));
    }
}
