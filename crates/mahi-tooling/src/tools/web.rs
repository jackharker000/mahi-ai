//! Built-in `web_fetch` and `web_search` tools.
//!
//! The actual HTTP paths (reqwest) sit behind the `net` cargo feature so the
//! crate builds and tests fully offline. Without `net`, the tools are still
//! registered and report an honest, non-retryable error. The response-parsing
//! helpers compile unconditionally and are unit-tested without any network.

use crate::tool::{ok_result, parse_args, tool_error, Tool};
use async_trait::async_trait;
use mahi_contracts::tooling::{DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Web tools require network: every mode except on-device (which gets a
/// cached/partial variant later — see mode matrix row "Web search/fetch").
const WEB_MODES: [ComputeMode; 3] =
    [ComputeMode::MacLan, ComputeMode::MacRemote, ComputeMode::Hosted];

/// Cap on fetched body size surfaced to the model.
const MAX_BODY_BYTES: usize = 256 * 1024;

#[cfg(not(feature = "net"))]
const NET_DISABLED_MSG: &str =
    "mahi-tooling was built without the `net` feature; web tools are unavailable";

// ---------------------------------------------------------------------------
// Parsing helpers (no network; compiled unconditionally)
// ---------------------------------------------------------------------------

/// One parsed search result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SearchHit {
    pub url: String,
    pub title: String,
    pub snippet: Option<String>,
}

/// Extract the `<title>` text from an HTML document (ASCII case-insensitive).
pub(crate) fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let gt = lower[open..].find('>')? + open + 1;
    let close = lower[gt..].find("</title")? + gt;
    let title = html.get(gt..close)?.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Strip tags (including `<script>`/`<style>` bodies) and collapse whitespace.
/// Deliberately naive: good enough for excerpting, not a real HTML parser.
pub(crate) fn strip_tags(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip script/style blocks entirely.
            if lower[i..].starts_with("<script") {
                i = lower[i..].find("</script").map(|j| i + j).unwrap_or(bytes.len());
                continue;
            }
            if lower[i..].starts_with("<style") {
                i = lower[i..].find("</style").map(|j| i + j).unwrap_or(bytes.len());
                continue;
            }
            // Skip to the end of this tag.
            i = html[i..].find('>').map(|j| i + j + 1).unwrap_or(bytes.len());
            out.push(' ');
            continue;
        }
        // Copy the full UTF-8 character.
        let ch_len = utf8_len(bytes[i]);
        if let Some(s) = html.get(i..i + ch_len) {
            out.push_str(s);
        }
        i += ch_len;
    }
    // Collapse whitespace runs.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_was_space = true;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                collapsed.push(' ');
            }
            last_was_space = true;
        } else {
            collapsed.push(ch);
            last_was_space = false;
        }
    }
    collapsed.trim().to_string()
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        b if b >> 3 == 0b11110 => 4,
        _ => 1, // continuation byte / invalid: advance one
    }
}

/// Parse a DuckDuckGo Instant Answer API response (`format=json`) into hits.
/// Handles `AbstractURL`/`Heading`/`AbstractText` plus flat and nested
/// `RelatedTopics` entries.
pub(crate) fn parse_search_response(body: &serde_json::Value, max: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();

    let abstract_url = body.get("AbstractURL").and_then(|v| v.as_str()).unwrap_or("");
    if !abstract_url.is_empty() {
        hits.push(SearchHit {
            url: abstract_url.to_string(),
            title: body
                .get("Heading")
                .and_then(|v| v.as_str())
                .unwrap_or(abstract_url)
                .to_string(),
            snippet: body
                .get("AbstractText")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }

    fn collect(topics: &[serde_json::Value], hits: &mut Vec<SearchHit>, max: usize) {
        for topic in topics {
            if hits.len() >= max {
                return;
            }
            if let Some(nested) = topic.get("Topics").and_then(|v| v.as_array()) {
                collect(nested, hits, max);
                continue;
            }
            let Some(url) = topic.get("FirstURL").and_then(|v| v.as_str()) else { continue };
            let text = topic.get("Text").and_then(|v| v.as_str()).unwrap_or(url);
            hits.push(SearchHit {
                url: url.to_string(),
                // The Text field is "Title - snippet" style; use the first
                // sentence-ish chunk as the title.
                title: text.split(" - ").next().unwrap_or(text).to_string(),
                snippet: Some(text.to_string()),
            });
        }
    }

    if let Some(topics) = body.get("RelatedTopics").and_then(|v| v.as_array()) {
        collect(topics, &mut hits, max);
    }
    hits.truncate(max);
    hits
}

/// Validate a fetchable URL scheme.
pub(crate) fn validate_url(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!("unsupported URL (must be http:// or https://): {url}"))
    }
}

/// Minimal percent-encoding for a query-string value.
#[cfg_attr(not(feature = "net"), allow(dead_code))]
pub(crate) fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Build the events for a fetched page: a citation followed by the result.
#[cfg_attr(not(feature = "net"), allow(dead_code))]
pub(crate) fn fetch_events(url: &str, status: u16, html: &str) -> Vec<ToolEvent> {
    let title = extract_title(html);
    let text = strip_tags(html);
    let truncated = text.len() > MAX_BODY_BYTES;
    let mut body: String = text;
    if truncated {
        let mut end = MAX_BODY_BYTES;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    let excerpt: String = body.chars().take(200).collect();
    vec![
        ToolEvent::Citation {
            url: url.to_string(),
            title: title.clone(),
            excerpt: Some(excerpt),
        },
        ToolEvent::Result {
            output: json!({
                "url": url,
                "status": status,
                "title": title,
                "text": body,
            }),
            truncated,
        },
    ]
}

/// Build the events for a search: one citation per hit, then the result.
#[cfg_attr(not(feature = "net"), allow(dead_code))]
pub(crate) fn search_events(query: &str, hits: &[SearchHit]) -> Vec<ToolEvent> {
    let mut events: Vec<ToolEvent> = hits
        .iter()
        .map(|hit| ToolEvent::Citation {
            url: hit.url.clone(),
            title: Some(hit.title.clone()),
            excerpt: hit.snippet.clone(),
        })
        .collect();
    events.push(ToolEvent::Result {
        output: json!({
            "query": query,
            "results": hits,
        }),
        truncated: false,
    });
    events
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct WebFetchTool;

#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "web_fetch".to_string(),
            display_name: "Fetch Web Page".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: WEB_MODES.to_vec(),
            required_permissions: vec!["net.fetch".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "http(s) URL to fetch" }
                },
                "required": ["url"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "status": { "type": "integer" },
                    "title": { "type": ["string", "null"] },
                    "text": { "type": "string" }
                }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: WebFetchArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if let Err(msg) = validate_url(&args.url) {
            return tool_error(msg, false);
        }

        #[cfg(not(feature = "net"))]
        {
            tool_error(NET_DISABLED_MSG, false)
        }

        #[cfg(feature = "net")]
        {
            match net::get_text(&args.url).await {
                Ok((status, body)) => crate::tool::events(
                    fetch_events(&args.url, status, &body).into_iter().map(Ok).collect(),
                ),
                Err(msg) => tool_error(msg, true),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct WebSearchTool;

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "web_search".to_string(),
            display_name: "Web Search".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: WEB_MODES.to_vec(),
            required_permissions: vec!["net.search".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "default": 10 }
                },
                "required": ["query"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "results": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "url": { "type": "string" },
                                "title": { "type": "string" },
                                "snippet": { "type": ["string", "null"] }
                            }
                        }
                    }
                }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: WebSearchArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.query.trim().is_empty() {
            return tool_error("query must not be empty", false);
        }

        #[cfg(not(feature = "net"))]
        {
            tool_error(NET_DISABLED_MSG, false)
        }

        #[cfg(feature = "net")]
        {
            let max = args.max_results.unwrap_or(10).clamp(1, 25);
            let url = format!(
                "https://api.duckduckgo.com/?q={}&format=json&no_html=1&no_redirect=1",
                urlencode(&args.query)
            );
            match net::get_json(&url).await {
                Ok(body) => {
                    let hits = parse_search_response(&body, max);
                    crate::tool::events(
                        search_events(&args.query, &hits).into_iter().map(Ok).collect(),
                    )
                }
                Err(msg) => tool_error(msg, true),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP layer (net feature only)
// ---------------------------------------------------------------------------

#[cfg(feature = "net")]
mod net {
    use std::time::Duration;

    fn client() -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("mahi-ai/0.1 (+https://github.com/jackharker000/mahi-ai)")
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))
    }

    pub(super) async fn get_text(url: &str) -> Result<(u16, String), String> {
        let resp = client()?
            .get(url)
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| format!("failed reading body of {url}: {e}"))?;
        Ok((status, body))
    }

    pub(super) async fn get_json(url: &str) -> Result<serde_json::Value, String> {
        let (_status, body) = get_text(url).await?;
        serde_json::from_str(&body).map_err(|e| format!("non-JSON response from {url}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_case_insensitively() {
        let html = "<html><head><TITLE> Mahi AI — docs </TITLE></head><body></body></html>";
        assert_eq!(extract_title(html), Some("Mahi AI — docs".to_string()));
        assert_eq!(extract_title("<html><body>no title</body></html>"), None);
    }

    #[test]
    fn strips_tags_scripts_and_collapses_whitespace() {
        let html = "<html><head><script>var x = '<b>not text</b>';</script></head>\
                    <body><h1>Hello</h1>\n\n  <p>world &amp; co</p></body></html>";
        let text = strip_tags(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("not text"));
        assert!(!text.contains("  "), "whitespace must be collapsed: {text:?}");
    }

    #[test]
    fn parses_ddg_search_response_including_nested_topics() {
        let body = serde_json::json!({
            "Heading": "Rust (programming language)",
            "AbstractText": "Rust is a systems programming language.",
            "AbstractURL": "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "RelatedTopics": [
                { "FirstURL": "https://example.com/a", "Text": "Result A - first related" },
                { "Topics": [
                    { "FirstURL": "https://example.com/b", "Text": "Result B - nested related" }
                ]},
                { "Name": "category-without-url" }
            ]
        });
        let hits = parse_search_response(&body, 10);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].url, "https://en.wikipedia.org/wiki/Rust_(programming_language)");
        assert_eq!(hits[0].title, "Rust (programming language)");
        assert_eq!(hits[1].title, "Result A");
        assert_eq!(hits[2].url, "https://example.com/b");
    }

    #[test]
    fn search_response_respects_max() {
        let body = serde_json::json!({
            "RelatedTopics": [
                { "FirstURL": "https://example.com/1", "Text": "one" },
                { "FirstURL": "https://example.com/2", "Text": "two" },
                { "FirstURL": "https://example.com/3", "Text": "three" }
            ]
        });
        assert_eq!(parse_search_response(&body, 2).len(), 2);
    }

    #[test]
    fn fetch_events_emit_citation_then_result() {
        let events = fetch_events("https://example.com", 200, "<title>Example</title><p>Body text</p>");
        assert_eq!(events.len(), 2);
        match &events[0] {
            ToolEvent::Citation { url, title, excerpt } => {
                assert_eq!(url, "https://example.com");
                assert_eq!(title.as_deref(), Some("Example"));
                assert!(excerpt.as_deref().unwrap_or_default().contains("Body text"));
            }
            other => panic!("expected citation, got {other:?}"),
        }
        match &events[1] {
            ToolEvent::Result { output, truncated } => {
                assert!(!truncated);
                assert_eq!(output["status"], 200);
                assert_eq!(output["title"], "Example");
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn search_events_emit_one_citation_per_hit_then_result() {
        let hits = vec![
            SearchHit { url: "https://a.example".into(), title: "A".into(), snippet: None },
            SearchHit { url: "https://b.example".into(), title: "B".into(), snippet: Some("bee".into()) },
        ];
        let events = search_events("letters", &hits);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], ToolEvent::Citation { url, .. } if url == "https://a.example"));
        assert!(matches!(&events[1], ToolEvent::Citation { url, .. } if url == "https://b.example"));
        match &events[2] {
            ToolEvent::Result { output, .. } => {
                assert_eq!(output["query"], "letters");
                assert_eq!(output["results"].as_array().map(Vec::len), Some(2));
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn validates_url_scheme() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn urlencodes_query_values() {
        assert_eq!(urlencode("rust async streams"), "rust+async+streams");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
