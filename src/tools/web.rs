//! `web` — search the internet and read a page.
//!
//! Two actions because they're two different needs: `search` finds candidate
//! URLs, `fetch` turns one into text the model can actually read. Output is
//! kept small on purpose — the target model class has a modest context, and a
//! raw HTML page is mostly markup.
//!
//! Search needs a provider (config `[web]`); fetch needs nothing. That split is
//! deliberate: reading a URL the user pasted works out of the box.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput};
use crate::config::{Config, WebProvider};

/// Cap on extracted page text handed back to the model.
const MAX_PAGE_CHARS: usize = 12_000;
const TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("worksmith/", env!("CARGO_PKG_VERSION"));

pub struct WebTool;

#[async_trait]
impl Tool for WebTool {
    fn name(&self) -> &str {
        "web"
    }

    fn description(&self) -> &str {
        "Search the web, or fetch a URL as readable text. Use it for anything outside this \
         machine: current documentation, library versions, error messages you don't recognize. \
         Prefer fetching a specific page over searching when you already have the URL."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "fetch"] },
                "query": { "type": "string", "description": "search terms (action=search)" },
                "url": { "type": "string", "description": "absolute URL (action=fetch)" },
                "limit": { "type": "integer", "description": "results to return, default 5" }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        match args.get("action").and_then(|v| v.as_str()).unwrap_or("search") {
            "search" => {
                let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                    return ToolOutput::error("missing required argument: query");
                };
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5)
                    .clamp(1, 10) as usize;
                let cfg = match Config::load(&ctx.cwd) {
                    Ok(c) => c,
                    Err(e) => return ToolOutput::error(format!("config error: {e}")),
                };
                match search(&cfg, query, limit).await {
                    Ok(results) if results.is_empty() => ToolOutput::ok("(no results)"),
                    Ok(results) => {
                        let mut out = String::new();
                        for r in results {
                            out.push_str(&format!("{}\n{}\n{}\n\n", r.title, r.url, r.snippet));
                        }
                        ToolOutput::ok(out)
                    }
                    Err(e) => ToolOutput::error(e),
                }
            }
            "fetch" => {
                let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
                    return ToolOutput::error("missing required argument: url");
                };
                match fetch(url).await {
                    Ok(text) => ToolOutput::ok(text),
                    Err(e) => ToolOutput::error(e),
                }
            }
            other => ToolOutput::error(format!("unknown action `{other}` (search|fetch)")),
        }
    }
}

pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Query the configured search provider. Providers differ only in their request
/// shape and where the results live in the response.
async fn search(cfg: &Config, query: &str, limit: usize) -> Result<Vec<SearchResult>, String> {
    let web = cfg.web();
    let Some(provider) = web.provider else {
        return Err(
            "no web search provider configured. Add a [web] section to config.toml, e.g.\n\
             [web]\nprovider = \"brave\"\napi-key-env = \"BRAVE_API_KEY\"\n\
             (providers: brave, tavily, searxng — searxng needs base-url, not a key)"
                .to_string(),
        );
    };
    let key = web
        .api_key_env
        .as_ref()
        .and_then(|e| std::env::var(e).ok());

    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let json: Value = match provider {
        WebProvider::Brave => {
            let key = key.ok_or_else(|| {
                "brave search needs an API key; set `api-key-env` in [web] and export it"
                    .to_string()
            })?;
            let url = web
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.search.brave.com/res/v1/web/search".into());
            client
                .get(url)
                .header("X-Subscription-Token", key)
                .header("Accept", "application/json")
                .query(&[("q", query), ("count", &limit.to_string())])
                .send()
                .await
                .map_err(|e| format!("search request failed: {e}"))?
                .json()
                .await
                .map_err(|e| format!("bad search response: {e}"))?
        }
        WebProvider::Tavily => {
            let key = key.ok_or_else(|| {
                "tavily needs an API key; set `api-key-env` in [web] and export it".to_string()
            })?;
            let url = web
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.tavily.com/search".into());
            client
                .post(url)
                .json(&json!({
                    "api_key": key,
                    "query": query,
                    "max_results": limit,
                }))
                .send()
                .await
                .map_err(|e| format!("search request failed: {e}"))?
                .json()
                .await
                .map_err(|e| format!("bad search response: {e}"))?
        }
        WebProvider::Searxng => {
            let base = web
                .base_url
                .clone()
                .ok_or_else(|| "searxng needs `base-url` in [web] (your instance)".to_string())?;
            client
                .get(format!("{}/search", base.trim_end_matches('/')))
                .query(&[("q", query), ("format", "json")])
                .send()
                .await
                .map_err(|e| format!("search request failed: {e}"))?
                .json()
                .await
                .map_err(|e| format!("bad search response: {e}"))?
        }
    };

    Ok(parse_results(provider, &json, limit))
}

/// Pull results out of a provider's JSON. Kept separate (and pure) so the
/// shapes are testable without network access.
pub fn parse_results(provider: WebProvider, json: &Value, limit: usize) -> Vec<SearchResult> {
    let items = match provider {
        WebProvider::Brave => json.get("web").and_then(|w| w.get("results")),
        WebProvider::Tavily => json.get("results"),
        WebProvider::Searxng => json.get("results"),
    };
    let Some(items) = items.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .take(limit)
        .map(|it| {
            let snippet = it
                .get("description")
                .or_else(|| it.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            SearchResult {
                title: it.get("title").and_then(|v| v.as_str()).unwrap_or("(untitled)").to_string(),
                url: it.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                snippet: strip_tags(snippet),
            }
        })
        .filter(|r| !r.url.is_empty())
        .collect()
}

/// Fetch a URL and reduce it to readable text.
async fn fetch(url: &str) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("`{url}` is not an http(s) URL"));
    }
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client.get(url).send().await.map_err(|e| format!("fetch failed: {e}"))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.map_err(|e| format!("reading body failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }

    let text = if content_type.contains("html") || body.trim_start().starts_with('<') {
        html_to_text(&body)
    } else {
        body
    };
    Ok(cap(&text, MAX_PAGE_CHARS))
}

/// A deliberately small HTML-to-text pass: drop script/style bodies, turn tags
/// into whitespace, unescape the handful of entities that actually matter.
/// Feeding a model markup wastes its context; full fidelity isn't the point.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let lower = html.to_lowercase();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip the entire contents of non-prose elements.
            for tag in ["script", "style", "head", "svg", "noscript"] {
                if lower[i..].starts_with(&format!("<{tag}")) {
                    let close = format!("</{tag}>");
                    if let Some(end) = lower[i..].find(&close) {
                        i += end + close.len();
                    } else {
                        i = bytes.len();
                    }
                    out.push('\n');
                    continue;
                }
            }
            if i >= bytes.len() {
                break;
            }
            match lower[i..].find('>') {
                Some(end) => {
                    // Block-level tags become line breaks so structure survives.
                    let tag = &lower[i..i + end];
                    if ["<p", "<br", "<div", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "</p", "</div", "</li", "</h"]
                        .iter()
                        .any(|t| tag.starts_with(t))
                    {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                    i += end + 1;
                }
                None => break,
            }
            continue;
        }
        let ch = html[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    unescape(&squeeze(&out))
}

/// Collapse runs of blank lines and trailing spaces.
fn squeeze(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn strip_tags(s: &str) -> String {
    html_to_text(s).replace('\n', " ")
}

fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n\n[page truncated at {max} characters]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text() {
        let html = "<html><head><title>x</title><style>p{color:red}</style></head>\
                    <body><h1>Title</h1><p>First &amp; best.</p>\
                    <script>alert('no')</script><p>Second.</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("First & best."), "entities unescaped: {text}");
        assert!(text.contains("Second."));
        assert!(!text.contains("alert"), "script bodies are dropped: {text}");
        assert!(!text.contains("color:red"), "style bodies are dropped: {text}");
        assert!(!text.contains('<'), "no markup survives: {text}");
    }

    #[test]
    fn page_text_is_capped() {
        let long = "word ".repeat(MAX_PAGE_CHARS);
        let out = cap(&long, MAX_PAGE_CHARS);
        assert!(out.chars().count() < MAX_PAGE_CHARS + 100);
        assert!(out.contains("truncated"));
    }

    #[test]
    fn provider_shapes_parse() {
        let brave = serde_json::json!({"web": {"results": [
            {"title": "A", "url": "https://a", "description": "<b>snip</b>"},
            {"title": "B", "url": "https://b", "description": "two"}
        ]}});
        let got = parse_results(WebProvider::Brave, &brave, 5);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].url, "https://a");
        assert_eq!(got[0].snippet, "snip", "markup stripped from snippets");

        let tavily = serde_json::json!({"results": [
            {"title": "T", "url": "https://t", "content": "body"}
        ]});
        let got = parse_results(WebProvider::Tavily, &tavily, 5);
        assert_eq!(got[0].snippet, "body");

        // A result with no URL is useless to the model.
        let junk = serde_json::json!({"results": [{"title": "no url"}]});
        assert!(parse_results(WebProvider::Searxng, &junk, 5).is_empty());
        // An unexpected shape must not panic.
        assert!(parse_results(WebProvider::Brave, &serde_json::json!({}), 5).is_empty());
    }
}
