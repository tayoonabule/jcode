use super::{Tool, ToolContext, ToolOutput};
use crate::config::WebSearchEngine;
use anyhow::Result;
use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// A search provider must fail quickly enough for the configured fallback to
/// be useful. Public HTML endpoints can accept a connection and then stall
/// while presenting a bot challenge, so a connect timeout is not enough.
const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Web search using DuckDuckGo or Bing (HTML scraping, with optional Bing API)
pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            client: crate::provider::shared_http_client(),
        }
    }
}

#[derive(Deserialize)]
struct WebSearchInput {
    query: String,
    #[serde(default)]
    num_results: Option<usize>,
    #[serde(default)]
    engine: Option<WebSearchEngine>,
    #[serde(default)]
    bing_market: Option<String>,
}

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Clone, Copy)]
struct BingSearchOptions<'a> {
    market: &'a str,
    configured_api_key: Option<&'a str>,
    api_key_env: &'a str,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }

    fn description(&self) -> &str {
        jcode_message_types::provider_native::LOCAL_WEBSEARCH_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "intent": super::intent_schema_property(),
                "query": {
                    "type": "string",
                    "description": "Search query."
                },
                "num_results": {
                    "type": "integer",
                    "description": "Max results."
                },
                "engine": {
                    "type": "string",
                    "enum": ["duckduckgo", "bing", "searxng"],
                    "description": "Engine. Defaults to duckduckgo; bing uses JCODE_BING_API_KEY, searxng uses JCODE_SEARXNG_URL."
                },
                "bing_market": {
                    "type": "string",
                    "description": "Optional Bing market, e.g. en-US or zh-CN. Defaults to JCODE_BING_MARKET or en-US."
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let params: WebSearchInput = serde_json::from_value(input)?;
        let num_results = params.num_results.unwrap_or(8).min(20);

        let config = crate::config::config();
        let primary_engine = params.engine.unwrap_or(config.websearch.engine);
        let engines = local_engine_order(
            primary_engine,
            &config.websearch.fallback_engines,
        );

        let market = params
            .bing_market
            .as_deref()
            .unwrap_or(&config.websearch.bing_market);
        let mut last_error = None;
        for (index, engine) in engines.into_iter().enumerate() {
            let allow_bing_api = index == 0;
            match self
                .search_with_engine(
                    engine,
                    &params.query,
                    num_results,
                    BingSearchOptions {
                        market,
                        configured_api_key: config.websearch.bing_api_key.as_deref(),
                        api_key_env: &config.websearch.bing_api_key_env,
                    },
                    allow_bing_api,
                )
                .await
            {
                Ok(found) => {
                    if !found.is_empty() {
                        return Ok(ToolOutput::new(format_search_results(
                            &params.query,
                            &found,
                            primary_engine,
                            engine,
                        )));
                    }
                }
                Err(err) => last_error = Some(err),
            }
        }

        if let Some(err) = last_error {
            return Err(err);
        }

        Ok(ToolOutput::new(format!(
            "No results found for: {}\n\n\
                 If results are consistently empty on this machine, the default \
                 DuckDuckGo/Bing engines may be blocked here by TLS fingerprinting \
                 or IP reputation (common on Linux/servers). Workarounds:\n\
                 - Point at a SearXNG instance: set `websearch.searxng_url` (or \
                 JCODE_SEARXNG_URL) and use engine \"searxng\".\n\
                 - Or provide a Bing Search API key via JCODE_BING_API_KEY.",
            params.query
        )))
    }
}

fn format_search_results(
    query: &str,
    results: &[SearchResult],
    primary_engine: WebSearchEngine,
    used_engine: WebSearchEngine,
) -> String {
    let provenance = if used_engine == primary_engine {
        format!("Engine: {} (primary)", used_engine.as_str())
    } else {
        format!(
            "Engine: {} (fallback from {})",
            used_engine.as_str(),
            primary_engine.as_str()
        )
    };
    let mut output = format!("Search results for: {query}\n{provenance}\n\n");
    for (i, result) in results.iter().enumerate() {
        output.push_str(&format!(
            "{}. **{}**\n   {}\n   {}\n\n",
            i + 1,
            result.title,
            result.url,
            result.snippet
        ));
    }
    output
}

fn search_engine_order(
    primary: WebSearchEngine,
    fallbacks: &[WebSearchEngine],
) -> Vec<WebSearchEngine> {
    let mut engines = vec![primary];
    for &engine in fallbacks {
        if !engines.contains(&engine) {
            engines.push(engine);
        }
    }
    engines
}

impl WebSearchTool {
    async fn search_with_engine(
        &self,
        engine: WebSearchEngine,
        query: &str,
        num_results: usize,
        bing: BingSearchOptions<'_>,
        allow_bing_api: bool,
    ) -> Result<Vec<SearchResult>> {
        match engine {
            WebSearchEngine::Duckduckgo => self.search_duckduckgo(query, num_results).await,
            WebSearchEngine::Bing => {
                self.search_bing(query, num_results, bing, allow_bing_api)
                    .await
            }
            WebSearchEngine::Searxng => self.search_searxng(query, num_results).await,
            // Provider-native search never reaches the local tool: engine order
            // filters it out. Kept for exhaustiveness.
            WebSearchEngine::Native => Ok(Vec::new()),
        }
    }

    async fn search_duckduckgo(
        &self,
        query: &str,
        num_results: usize,
    ) -> Result<Vec<SearchResult>> {
        // DuckDuckGo's HTML endpoint now serves an anti-bot "anomaly" challenge
        // (HTTP 202, no results) for plain GET requests. Submitting the query as
        // a POST form, the same way the real HTML page does, still returns the
        // standard results markup with a 200.
        let response = self
            .client
            .post("https://html.duckduckgo.com/html/")
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .form(&[("q", query), ("kl", "us-en")])
            .timeout(ENGINE_REQUEST_TIMEOUT)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Search failed with status: {}",
                response.status()
            ));
        }

        let body = response.text().await?;
        let results = parse_ddg_results(&body, num_results);
        if results.is_empty()
            && let Some(reason) = detect_anti_bot_page(&body)
        {
            return Err(anyhow::anyhow!(
                "DuckDuckGo served an anti-bot challenge page ({reason}) instead of \
                 results. This is commonly caused by TLS fingerprinting or IP \
                 reputation on Linux. Falling back to another engine if configured."
            ));
        }

        Ok(results)
    }

    async fn search_bing(
        &self,
        query: &str,
        num_results: usize,
        options: BingSearchOptions<'_>,
        allow_api: bool,
    ) -> Result<Vec<SearchResult>> {
        if allow_api {
            if let Some(api_key) = options
                .configured_api_key
                .filter(|key| !key.trim().is_empty())
            {
                return self
                    .search_bing_api(query, num_results, options.market, api_key)
                    .await;
            }
            if let Ok(api_key) = std::env::var(options.api_key_env)
                && !api_key.trim().is_empty()
            {
                return self
                    .search_bing_api(query, num_results, options.market, &api_key)
                    .await;
            }
        }

        // Bing's HTML SERP is frequently reshaped and can require JavaScript
        // or redirect decoding. Its RSS endpoint is intentionally small and
        // has remained stable, so prefer it for the keyless path and retain
        // HTML as a compatibility fallback for instances that disable RSS.
        match self
            .search_bing_rss(query, num_results, options.market)
            .await
        {
            Ok(results) if !results.is_empty() => Ok(results),
            Ok(_) => {
                self.search_bing_html(query, num_results, options.market)
                    .await
            }
            Err(rss_err) => match self
                .search_bing_html(query, num_results, options.market)
                .await
            {
                Ok(results) => Ok(results),
                Err(html_err) => Err(anyhow::anyhow!(
                    "Bing search failed via RSS ({rss_err}) and HTML ({html_err})"
                )),
            },
        }
    }

    async fn search_bing_rss(
        &self,
        query: &str,
        num_results: usize,
        market: &str,
    ) -> Result<Vec<SearchResult>> {
        let response = self
            .client
            .get("https://www.bing.com/search")
            .query(&[("format", "rss"), ("q", query), ("mkt", market)])
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
            )
            .header(
                reqwest::header::ACCEPT,
                "application/rss+xml, application/xml",
            )
            .timeout(ENGINE_REQUEST_TIMEOUT)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Bing RSS search failed with status: {}",
                response.status()
            ));
        }

        let body = response.text().await?;
        Ok(parse_bing_rss_results(&body, num_results))
    }

    async fn search_bing_api(
        &self,
        query: &str,
        num_results: usize,
        market: &str,
        api_key: &str,
    ) -> Result<Vec<SearchResult>> {
        let response = self
            .client
            .get("https://api.bing.microsoft.com/v7.0/search")
            // `responseFilter=Webpages` keeps the result budget on the one
            // answer type this tool presents; Bing otherwise mixes in others.
            .query(&[
                ("q", query),
                ("count", &num_results.to_string()),
                ("mkt", market),
                ("responseFilter", "Webpages"),
            ])
            .header("Ocp-Apim-Subscription-Key", api_key)
            .timeout(ENGINE_REQUEST_TIMEOUT)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Bing API search failed with status: {}",
                response.status()
            ));
        }

        Ok(parse_bing_api_results(response.json().await?, num_results))
    }

    fn bing_html_request(&self, query: &str, market: &str) -> reqwest::RequestBuilder {
        let url = format!(
            "https://www.bing.com/search?q={}&mkt={}",
            urlencoding::encode(query),
            urlencoding::encode(market)
        );
        self.client
            .get(&url)
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
            )
            .timeout(ENGINE_REQUEST_TIMEOUT)
    }

    async fn search_bing_html(
        &self,
        query: &str,
        num_results: usize,
        market: &str,
    ) -> Result<Vec<SearchResult>> {
        let response = self.bing_html_request(query, market).send().await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Bing search failed with status: {}",
                response.status()
            ));
        }

        let body = response.text().await?;
        let results = parse_bing_html_results(&body, num_results);
        if results.is_empty()
            && let Some(reason) = detect_anti_bot_page(&body)
        {
            return Err(anyhow::anyhow!(
                "Bing served an anti-bot challenge page ({reason}) instead of results."
            ));
        }

        Ok(results)
    }

    /// Query a user-configured SearXNG instance via its JSON API. SearXNG is a
    /// self-hostable metasearch engine; because the request goes to an instance
    /// the user controls (or a public one they trust), it sidesteps the TLS
    /// fingerprinting / IP-reputation blocks that DuckDuckGo and Bing apply to
    /// scraped requests on some hosts (see issue #270).
    async fn search_searxng(&self, query: &str, num_results: usize) -> Result<Vec<SearchResult>> {
        let config = crate::config::config();
        let base = config
            .websearch
            .searxng_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map(|u| u.to_string())
            .or_else(|| {
                std::env::var(&config.websearch.searxng_url_env)
                    .ok()
                    .filter(|u| !u.trim().is_empty())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "SearXNG engine selected but no instance URL configured. Set \
                     `websearch.searxng_url` in your config or the {} environment \
                     variable to a SearXNG base URL (e.g. https://searx.example.org).",
                    config.websearch.searxng_url_env
                )
            })?;

        let endpoint = format!("{}/search", base.trim_end_matches('/'));
        let response = self
            .client
            .get(&endpoint)
            .query(&[("q", query), ("format", "json")])
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(ENGINE_REQUEST_TIMEOUT)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "SearXNG search failed with status {} (endpoint: {endpoint}). \
                 Ensure the instance has the JSON format enabled in its settings.",
                response.status()
            ));
        }

        let parsed: SearxngResponse = response.json().await.map_err(|err| {
            anyhow::anyhow!(
                "SearXNG returned a non-JSON response ({err}). The instance may have \
                 the JSON format disabled; enable `formats: [html, json]` in its settings."
            )
        })?;

        Ok(parse_searxng_results(parsed, num_results))
    }
}

/// Map a parsed SearXNG JSON response to `SearchResult`s, dropping entries with
/// empty URLs and capping to `num_results`.
fn parse_searxng_results(response: SearxngResponse, num_results: usize) -> Vec<SearchResult> {
    response
        .results
        .into_iter()
        .filter(|r| is_http_url(&r.url))
        .take(num_results)
        .map(|r| SearchResult {
            title: if r.title.trim().is_empty() {
                r.url.clone()
            } else {
                r.title
            },
            url: r.url,
            snippet: r.content.unwrap_or_default(),
        })
        .collect()
}

mod search_regex {
    use regex::Regex;
    use std::sync::OnceLock;

    fn compile_regex(pattern: &str, label: &str) -> Option<Regex> {
        match Regex::new(pattern) {
            Ok(regex) => Some(regex),
            Err(err) => {
                crate::logging::warn(&format!(
                    "websearch: failed to compile static regex {label}: {}",
                    err
                ));
                None
            }
        }
    }

    macro_rules! static_regex {
        ($name:ident, $pat:expr_2021) => {
            pub fn $name() -> Option<&'static Regex> {
                static RE: OnceLock<Option<Regex>> = OnceLock::new();
                RE.get_or_init(|| compile_regex($pat, stringify!($name)))
                    .as_ref()
            }
        };
    }

    static_regex!(
        result_link,
        r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#
    );
    static_regex!(
        result_snippet,
        r#"(?s)<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#
    );
    static_regex!(tag, r"<[^>]+>");
    static_regex!(
        bing_result_block,
        r#"(?s)<li[^>]*class="[^"]*\bb_algo\b[^"]*"[^>]*>(.*?)</li>"#
    );
    static_regex!(
        bing_link,
        r#"(?s)<h2[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>\s*</h2>"#
    );
    static_regex!(
        bing_caption,
        r#"(?s)<div[^>]*class="[^"]*\bb_caption\b[^"]*"[^>]*>.*?<p[^>]*>(.*?)</p>"#
    );
    static_regex!(rss_item, r#"(?s)<item\b[^>]*>(.*?)</item>"#);
    static_regex!(rss_title, r#"(?s)<title\b[^>]*>(.*?)</title>"#);
    static_regex!(rss_link, r#"(?s)<link\b[^>]*>(.*?)</link>"#);
    static_regex!(
        rss_description,
        r#"(?s)<description\b[^>]*>(.*?)</description>"#
    );
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct BingApiResponse {
    #[serde(rename = "webPages")]
    web_pages: Option<BingWebPages>,
}

#[derive(Deserialize)]
struct BingWebPages {
    value: Vec<BingWebPage>,
}

#[derive(Deserialize)]
struct BingWebPage {
    name: String,
    url: String,
    #[serde(default)]
    snippet: String,
}

fn parse_bing_api_results(response: BingApiResponse, max_results: usize) -> Vec<SearchResult> {
    response
        .web_pages
        .map(|pages| {
            pages
                .value
                .into_iter()
                .filter(|page| is_http_url(&page.url))
                .take(max_results)
                .map(|page| SearchResult {
                    title: page.name,
                    url: page.url,
                    snippet: page.snippet,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_bing_html_results(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let (Some(block_re), Some(link_re), Some(caption_re), Some(tag_re)) = (
        search_regex::bing_result_block(),
        search_regex::bing_link(),
        search_regex::bing_caption(),
        search_regex::tag(),
    ) else {
        return results;
    };

    for block in block_re.captures_iter(html) {
        if results.len() >= max_results {
            break;
        }
        let Some(link) = link_re.captures(&block[1]) else {
            continue;
        };
        let url = decode_bing_url(&link[1]);
        if !is_external_http_url(&url, "bing.com") {
            continue;
        }
        let title = html_decode(&tag_re.replace_all(&link[2], ""));
        let snippet = caption_re
            .captures(&block[1])
            .map(|cap| html_decode(&tag_re.replace_all(&cap[1], "")))
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }

    results
}

fn parse_bing_rss_results(xml: &str, max_results: usize) -> Vec<SearchResult> {
    let (Some(item_re), Some(title_re), Some(link_re), Some(description_re)) = (
        search_regex::rss_item(),
        search_regex::rss_title(),
        search_regex::rss_link(),
        search_regex::rss_description(),
    ) else {
        return Vec::new();
    };

    item_re
        .captures_iter(xml)
        .filter_map(|item| {
            let title = title_re
                .captures(&item[1])
                .map(|capture| html_decode(&capture[1]))?;
            let url = link_re
                .captures(&item[1])
                .map(|capture| html_decode(&capture[1]))?;
            if !is_external_http_url(&url, "bing.com") {
                return None;
            }
            let snippet = description_re
                .captures(&item[1])
                .map(|capture| html_decode(&capture[1]))
                .unwrap_or_default();
            Some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .take(max_results)
        .collect()
}

fn parse_ddg_results(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    let (Some(result_link), Some(result_snippet), Some(tag)) = (
        search_regex::result_link(),
        search_regex::result_snippet(),
        search_regex::tag(),
    ) else {
        return results;
    };

    let links: Vec<_> = result_link.captures_iter(html).collect();
    let snippets: Vec<_> = result_snippet.captures_iter(html).collect();

    for (i, link_cap) in links.iter().enumerate() {
        if results.len() >= max_results {
            break;
        }

        let url = decode_ddg_url(&link_cap[1]);
        let title = html_decode(&tag.replace_all(&link_cap[2], ""));

        if !is_external_http_url(&url, "duckduckgo.com") {
            continue;
        }

        let snippet = if i < snippets.len() {
            let raw = &snippets[i][1];
            html_decode(&tag.replace_all(raw, ""))
        } else {
            String::new()
        };

        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }

    results
}

/// Detect whether an HTML body is an anti-bot/captcha challenge rather than a
/// real results page. DuckDuckGo (and similar) serve these with HTTP 200, so a
/// successful status plus zero parsed results is ambiguous without this check.
///
/// Returns a short human-readable reason when a challenge page is detected.
fn detect_anti_bot_page(html: &str) -> Option<&'static str> {
    let lowered = html.to_ascii_lowercase();
    const MARKERS: &[(&str, &str)] = &[
        ("anomaly-modal", "anomaly challenge"),
        ("anomaly.js", "anomaly challenge"),
        ("dpn=1", "anomaly challenge"),
        ("captcha", "captcha"),
        ("g-recaptcha", "recaptcha"),
        ("are you a robot", "bot check"),
        ("unusual traffic", "bot check"),
        ("verify you are human", "human verification"),
        ("challenge-platform", "cloudflare challenge"),
        ("cf-challenge", "cloudflare challenge"),
    ];
    for (needle, reason) in MARKERS {
        if lowered.contains(needle) {
            return Some(reason);
        }
    }
    None
}

fn is_provider_host(url: &url::Url, domain: &str) -> bool {
    url.host_str().is_some_and(|host| {
        host == domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn is_external_http_url(value: &str, provider_domain: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && !is_provider_host(&url, provider_domain)
    })
}

fn is_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn decode_ddg_url(url: &str) -> String {
    // DDG wraps URLs like //duckduckgo.com/l/?uddg=ACTUAL_URL&...
    if let Some(uddg_start) = url.find("uddg=") {
        let start = uddg_start + 5;
        let end = url[start..]
            .find('&')
            .map(|i| start + i)
            .unwrap_or(url.len());
        let encoded = &url[start..end];
        urlencoding::decode(encoded)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| encoded.to_string())
    } else {
        url.to_string()
    }
}

/// Bing's public HTML results increasingly link through `bing.com/ck/a` and put
/// the destination in a URL-safe base64 `u=a1…` query value.  Treating every
/// Bing-owned URL as an ad used to discard all organic results on current SERPs.
fn decode_bing_url(url: &str) -> String {
    let decoded = html_decode(url);
    let redirect_url = if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded.clone()
    };
    let Ok(redirect) = url::Url::parse(&redirect_url) else {
        return decoded;
    };
    if !matches!(redirect.scheme(), "http" | "https")
        || !is_provider_host(&redirect, "bing.com")
        || !redirect.path().starts_with("/ck/")
    {
        return decoded;
    }

    let Some(encoded) = redirect
        .query_pairs()
        .find_map(|(key, value)| (key == "u").then_some(value))
    else {
        return decoded;
    };
    let encoded = encoded.strip_prefix("a1").unwrap_or(&encoded);

    for engine in [&URL_SAFE_NO_PAD, &URL_SAFE] {
        if let Ok(bytes) = engine.decode(encoded)
            && let Ok(destination) = String::from_utf8(bytes)
            && is_external_http_url(&destination, "bing.com")
        {
            return destination;
        }
    }

    decoded
}

fn html_decode(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .trim()
        .to_string()
}

/// Engines the local tool tries, in order. `native` is provider-side and never
/// runs locally: when it is preferred (e.g. the active provider has no server
/// search), the local fallbacks run, defaulting to DuckDuckGo then Bing.
fn local_engine_order(
    preferred: WebSearchEngine,
    fallbacks: &[WebSearchEngine],
) -> Vec<WebSearchEngine> {
    let mut engines: Vec<WebSearchEngine> = std::iter::once(preferred)
        .chain(fallbacks.iter().copied())
        .filter(|engine| engine.is_local())
        .collect();
    if engines.is_empty() {
        engines = vec![WebSearchEngine::Duckduckgo, WebSearchEngine::Bing];
    }
    let mut seen = std::collections::HashSet::new();
    engines.retain(|engine| seen.insert(*engine));
    engines
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_engine_falls_back_to_local_engines() {
        use super::local_engine_order;
        assert_eq!(
            local_engine_order(WebSearchEngine::Native, &[WebSearchEngine::Searxng]),
            vec![WebSearchEngine::Searxng]
        );
        assert_eq!(
            local_engine_order(WebSearchEngine::Native, &[WebSearchEngine::Native]),
            vec![WebSearchEngine::Duckduckgo, WebSearchEngine::Bing]
        );
        assert_eq!(
            local_engine_order(
                WebSearchEngine::Bing,
                &[WebSearchEngine::Duckduckgo, WebSearchEngine::Bing]
            ),
            vec![WebSearchEngine::Bing, WebSearchEngine::Duckduckgo]
        );
    }

    use super::*;

    #[test]
    fn bing_html_request_preserves_all_query_terms() {
        let tool = WebSearchTool {
            client: reqwest::Client::new(),
        };
        for query in [
            "rust async await",
            "rust async/await & tokio + 中文 #examples",
        ] {
            let request = tool.bing_html_request(query, "en-US").build().unwrap();
            assert_eq!(request.url().host_str(), Some("www.bing.com"));
            assert_eq!(request.url().path(), "/search");
            assert_eq!(
                request.url().query_pairs().collect::<Vec<_>>(),
                vec![("q".into(), query.into()), ("mkt".into(), "en-US".into())]
            );
            assert_eq!(request.timeout(), Some(&ENGINE_REQUEST_TIMEOUT));
        }
    }

    #[test]
    fn parses_bing_rss_results_and_ignores_channel_metadata() {
        let xml = r#"<?xml version="1.0"?>
            <rss><channel>
              <title>Bing: rust async await</title>
              <link>https://www.bing.com/search?q=rust</link>
              <item>
                <title>Rust &amp; Tokio</title>
                <link>https://tokio.rs/</link>
                <description>Async Rust runtime &amp; tools.</description>
              </item>
              <item>
                <title>Internal</title>
                <link>https://www.bing.com/search</link>
                <description>Not a result.</description>
              </item>
            </channel></rss>"#;

        let results = parse_bing_rss_results(xml, 8);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust & Tokio");
        assert_eq!(results[0].url, "https://tokio.rs/");
        assert_eq!(results[0].snippet, "Async Rust runtime & tools.");
    }

    #[test]
    fn bing_rss_results_respect_limit() {
        let xml = (1..=3)
            .map(|i| {
                format!(
                    "<item><title>Result {i}</title><link>https://example{i}.com/</link><description>Snippet</description></item>"
                )
            })
            .collect::<String>();
        assert_eq!(parse_bing_rss_results(&xml, 2).len(), 2);
    }

    #[test]
    fn search_output_attributes_the_successful_engine_without_claiming_relevance() {
        use WebSearchEngine::{Bing, Duckduckgo, Searxng};
        let results = vec![SearchResult {
            title: "Rust on Steam".to_string(),
            url: "https://store.steampowered.com/app/252490/Rust/".to_string(),
            snippet: "A survival game.".to_string(),
        }];
        for (primary, used, provenance) in [
            (Bing, Bing, "Engine: bing (primary)"),
            (Duckduckgo, Bing, "Engine: bing (fallback from duckduckgo)"),
            (Bing, Searxng, "Engine: searxng (fallback from bing)"),
        ] {
            assert_eq!(
                format_search_results("rust async await", &results, primary, used),
                format!(
                    "Search results for: rust async await\n{provenance}\n\n\
                     1. **Rust on Steam**\n   https://store.steampowered.com/app/252490/Rust/\n   A survival game.\n\n"
                )
            );
        }
    }

    #[test]
    fn fallback_order_does_not_retry_non_adjacent_engines() {
        use WebSearchEngine::{Bing, Duckduckgo, Searxng};
        assert_eq!(
            search_engine_order(Bing, &[Duckduckgo, Bing, Searxng, Duckduckgo]),
            vec![Bing, Duckduckgo, Searxng]
        );
    }

    #[test]
    fn engine_order_honors_the_configured_set_exactly() {
        use WebSearchEngine::{Bing, Duckduckgo, Searxng};
        // A self-hosted SearXNG user who removed Bing must not have Bing
        // silently appended: that would leak the query to an engine they
        // deliberately excluded and mask their own engine's errors.
        assert_eq!(
            search_engine_order(Searxng, &[Duckduckgo]),
            vec![Searxng, Duckduckgo]
        );
        assert_eq!(search_engine_order(Searxng, &[]), vec![Searxng]);
        assert!(!search_engine_order(Searxng, &[Duckduckgo]).contains(&Bing));
    }

    #[test]
    fn html_results_filter_provider_hosts_not_url_substrings() {
        for (domain, parser) in [
            (
                "bing.com",
                parse_bing_html_results as fn(&str, usize) -> Vec<SearchResult>,
            ),
            (
                "duckduckgo.com",
                parse_ddg_results as fn(&str, usize) -> Vec<SearchResult>,
            ),
        ] {
            let urls = [
                format!("https://example.org/review/{domain}"),
                format!("https://example.org/?source={domain}"),
                format!("https://not{domain}/"),
                format!("https://{domain}.example.org/"),
            ];
            for url in urls {
                let html = format!(
                    r#"<li class="b_algo"><h2><a class="result__a" href="{url}">Result</a></h2></li>"#
                );
                let results = parser(&html, 1);
                assert_eq!(results.len(), 1, "valid destination: {url}");
                assert_eq!(results[0].url, url);
            }
            for url in [
                format!("https://{domain}/ad"),
                format!("https://WWW.{}/ad", domain.to_ascii_uppercase()),
                "http-not-a-url".to_string(),
                "javascript:alert(1)".to_string(),
            ] {
                let html = format!(
                    r#"<li class="b_algo"><h2><a class="result__a" href="{url}">Result</a></h2></li>"#
                );
                assert!(parser(&html, 1).is_empty(), "invalid destination: {url}");
            }
        }
    }

    #[test]
    fn bing_redirect_decoding_requires_a_bing_host() {
        let encoded = URL_SAFE_NO_PAD.encode("https://destination.example/");
        let url = format!("https://example.org/bing.com/ck/a?u=a1{encoded}");
        assert_eq!(decode_bing_url(&url), url);
    }

    #[test]
    fn bing_redirect_variants_decode_to_external_http_urls() {
        let destination = "https://example.org/?q=bing.com&lang=en";
        for engine in [&URL_SAFE, &URL_SAFE_NO_PAD] {
            let encoded = engine.encode(destination);
            let encoded = urlencoding::encode(&encoded);
            for origin in [
                "https://www.bing.com",
                "https://WWW.BING.COM",
                "//www.bing.com",
            ] {
                let redirect = format!("{origin}/ck/a?ptn=3&amp;u=a1{encoded}#fragment");
                assert_eq!(decode_bing_url(&redirect), destination);
                let html =
                    format!(r#"<li class="b_algo"><h2><a href="{redirect}">Result</a></h2></li>"#);
                assert_eq!(parse_bing_html_results(&html, 1)[0].url, destination);
            }
        }
    }

    #[test]
    fn malformed_bing_redirects_are_not_results() {
        let mut redirects = vec![
            "https://www.bing.com/ck/a?u=%%%".to_string(),
            "https://www.bing.com/ck/a?missing=1".to_string(),
        ];
        for destination in [
            "http-not-a-url",
            "javascript:alert(1)",
            "https://www.bing.com/ad",
        ] {
            redirects.push(format!(
                "https://www.bing.com/ck/a?u=a1{}",
                URL_SAFE_NO_PAD.encode(destination)
            ));
        }
        for redirect in redirects {
            assert_eq!(decode_bing_url(&redirect), redirect);
            let html =
                format!(r#"<li class="b_algo"><h2><a href="{redirect}">Result</a></h2></li>"#);
            assert!(parse_bing_html_results(&html, 1).is_empty());
        }
    }

    #[test]
    fn parses_bing_html_results() {
        let html = r#"
            <li class="b_algo">
              <h2><a href="https://example.com/rust">Rust &amp; Cargo</a></h2>
              <div class="b_caption"><p>A <strong>systems</strong> language.</p></div>
            </li>
            <li class="b_algo"><h2><a href="https://www.bing.com/aclk">ad</a></h2></li>
            <li class="b_algo">
              <h2><a href="https://example.org/jcode">Jcode</a></h2>
              <div class="b_caption"><p>Agentic coding.</p></div>
            </li>
        "#;

        let results = parse_bing_html_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust & Cargo");
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(results[0].snippet, "A systems language.");
        assert_eq!(results[1].title, "Jcode");
    }

    #[test]
    fn parses_current_bing_redirect_results() {
        // Current Bing HTML wraps organic links in `bing.com/ck/a` and carries
        // the real destination as `u=a1` plus URL-safe base64 without padding.
        let html = r#"
            <li class="b_algo" data-id iid=SERP.100>
              <h2 class=""><a target="_blank" href="https://www.bing.com/ck/a?ptn=3&amp;u=a1aHR0cHM6Ly9wbGF5d3JpZ2h0LmRldi9kb2NzL3Rlc3Qtc25hcHNob3Rz&amp;ntb=1">Visual comparisons | Playwright</a></h2>
              <div class="b_caption"><p>Compare screenshots in Playwright.</p></div>
            </li>
        "#;

        let results = parse_bing_html_results(html, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://playwright.dev/docs/test-snapshots");
        assert_eq!(results[0].title, "Visual comparisons | Playwright");
    }

    #[test]
    fn parses_bing_api_results() {
        let response: BingApiResponse = serde_json::from_value(json!({
            "webPages": {
                "value": [
                    {"name": "Internal", "url": "file:///private/data", "snippet": "not a result"},
                    {"name": "One", "url": "https://one.test", "snippet": "first"},
                    {"name": "Two", "url": "https://two.test", "snippet": "second"}
                ]
            }
        }))
        .unwrap();

        let results = parse_bing_api_results(response, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "One");
        assert_eq!(results[0].url, "https://one.test");
    }

    #[test]
    fn parses_ddg_html_results() {
        // Mirrors the markup html.duckduckgo.com returns for the POST form,
        // where titles and snippets contain inline <b> highlight tags.
        let html = r#"
            <div class="result results_links results_links_deep web-result">
              <a class="result__a" href="https://rust-lang.org/"><b>Rust</b> Language</a>
              <a class="result__snippet" href="https://rust-lang.org/">A <b>systems</b> programming language.</a>
            </div>
            <div class="result results_links results_links_deep web-result">
              <a class="result__a" href="https://en.wikipedia.org/wiki/Rust">Rust on Wikipedia</a>
              <a class="result__snippet" href="https://en.wikipedia.org/wiki/Rust">Encyclopedia <b>entry</b>.</a>
            </div>
        "#;

        let results = parse_ddg_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[0].snippet, "A systems programming language.");
        assert_eq!(results[1].url, "https://en.wikipedia.org/wiki/Rust");
        assert_eq!(results[1].snippet, "Encyclopedia entry.");
    }

    #[test]
    fn websearch_engine_accepts_aliases() {
        assert_eq!(
            WebSearchEngine::parse("ddg"),
            Some(WebSearchEngine::Duckduckgo)
        );
        assert_eq!(WebSearchEngine::parse("bing"), Some(WebSearchEngine::Bing));
        assert_eq!(WebSearchEngine::parse("google"), None);
    }

    #[test]
    fn detects_ddg_anomaly_challenge_page() {
        // Shape of the anti-bot challenge DDG serves (HTTP 200) instead of
        // results when a request is flagged (e.g. TLS fingerprint on Linux).
        let html = r#"<!DOCTYPE html><html><head>
            <script src="/dist/anomaly.js"></script></head>
            <body><div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div>
            </body></html>"#;
        assert_eq!(detect_anti_bot_page(html), Some("anomaly challenge"));
        // And it should parse to zero real results.
        assert!(parse_ddg_results(html, 10).is_empty());
    }

    #[test]
    fn detects_generic_captcha_page() {
        let html = r#"<html><body><div class="g-recaptcha"></div>
            Please verify you are human.</body></html>"#;
        assert!(detect_anti_bot_page(html).is_some());
    }

    #[test]
    fn real_results_are_not_flagged_as_anti_bot() {
        let html = r#"
            <div class="result results_links web-result">
              <a class="result__a" href="https://rust-lang.org/">Rust</a>
              <a class="result__snippet" href="https://rust-lang.org/">A language.</a>
            </div>
        "#;
        assert_eq!(detect_anti_bot_page(html), None);
        assert_eq!(parse_ddg_results(html, 10).len(), 1);
    }

    // Captured from a live DuckDuckGo request that was flagged on Linux (GH #270):
    // the HTML endpoint returns HTTP 202 with an "anomaly" challenge page and no
    // results. These fixtures pin the real-world shapes so the fix stays honest.
    #[test]
    fn real_captured_ddg_anomaly_fixture_is_detected() {
        let html = include_str!("testdata/ddg_anomaly.html");
        // The bug: this page parses to zero real results...
        assert!(
            parse_ddg_results(html, 10).is_empty(),
            "anomaly page should yield no results"
        );
        // ...but the fix now recognizes it as a challenge instead of a silent
        // "no results found".
        assert_eq!(detect_anti_bot_page(html), Some("anomaly challenge"));
    }

    #[test]
    fn real_captured_ddg_results_fixture_parses() {
        let html = include_str!("testdata/ddg_results.html");
        assert_eq!(detect_anti_bot_page(html), None);
        assert!(
            !parse_ddg_results(html, 10).is_empty(),
            "real results page should yield results"
        );
    }

    #[test]
    fn parses_searxng_json_results() {
        // Shape of a real SearXNG /search?format=json response (#270).
        let body = serde_json::json!({
            "query": "rust",
            "results": [
                {
                    "url": "https://www.rust-lang.org/",
                    "title": "Rust Programming Language",
                    "content": "A language empowering everyone."
                },
                {
                    "url": "https://doc.rust-lang.org/book/",
                    "title": "The Rust Book",
                    "content": "Learn Rust."
                },
                // Entry with empty url is dropped; missing content tolerated.
                { "url": "", "title": "junk" },
                { "url": "javascript:alert(1)", "title": "junk" },
                { "url": "https://crates.io", "title": "" }
            ]
        });
        let parsed: SearxngResponse = serde_json::from_value(body).unwrap();
        let results = parse_searxng_results(parsed, 10);
        assert_eq!(results.len(), 3, "non-HTTP destinations should be dropped");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
        // Missing title falls back to the URL.
        assert_eq!(results[2].title, "https://crates.io");
        assert_eq!(results[2].snippet, "");
    }

    #[test]
    fn searxng_results_respect_limit() {
        let body = serde_json::json!({
            "results": (0..10)
                .map(|i| serde_json::json!({"url": format!("https://x/{i}"), "title": "t"}))
                .collect::<Vec<_>>()
        });
        let parsed: SearxngResponse = serde_json::from_value(body).unwrap();
        assert_eq!(parse_searxng_results(parsed, 3).len(), 3);
    }

    #[test]
    fn websearch_engine_parses_searxng_aliases() {
        assert_eq!(
            WebSearchEngine::parse("searxng"),
            Some(WebSearchEngine::Searxng)
        );
        assert_eq!(
            WebSearchEngine::parse("searx"),
            Some(WebSearchEngine::Searxng)
        );
        assert_eq!(WebSearchEngine::Searxng.as_str(), "searxng");
    }
}
