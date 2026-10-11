//! Web tools, ported from the `web_search`/`web_fetch` tools in `src/tools/index.ts`.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

/// Literal private/local host blocks, ported 1:1 from `isPrivateHost()`.
pub fn is_private_host(hostname: &str) -> bool {
    let host = hostname.to_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }
    if host == "::1" || host.starts_with("fc") || host.starts_with("fd") || host.starts_with("fe80:") {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|part| part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty()) {
        let a: u32 = parts[0].parse().unwrap_or(999);
        let b: u32 = parts[1].parse().unwrap_or(999);
        if a == 0 || a == 10 || a == 127 || (a == 100 && (64..=127).contains(&b)) || (a == 169 && b == 254) || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 198 && (b == 18 || b == 19)) {
            return true;
        }
    }
    if let Some(mapped) = host.strip_prefix("::ffff:") {
        return is_private_host(mapped);
    }
    false
}

fn http_client(timeout_ms: u64) -> reqwest::Client {
    reqwest::Client::builder().timeout(std::time::Duration::from_millis(timeout_ms)).build().unwrap_or_default()
}

/// Decode DuckDuckGo result links, ported from the TS extraction chain.
pub fn decode_ddg_href(href: &str) -> String {
    let stripped = href.strip_prefix("//duckduckgo.com/l/?uddg=").unwrap_or(href);
    let first_part = stripped.split('&').next().unwrap_or(stripped);
    urldecode(first_part)
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Strip tags and decode the handful of entities the TS `decodeHtml()` handles.
pub fn decode_html(value: &str) -> String {
    let mut without_tags = String::with_capacity(value.len());
    let mut depth = 0usize;
    for ch in value.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => without_tags.push(c),
            _ => {}
        }
    }
    without_tags
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Parse the DuckDuckGo HTML result listing, standalone for testability.
pub fn parse_ddg_results(html: &str, max_results: usize) -> Vec<String> {
    let pattern = regex::Regex::new(
        r#"(?is)<a[^>]+class="result__a"[^>]+href="([^"]+)"[^>]*>(.*?)</a>.*?<a[^>]+class="result__snippet"[^>]*>(.*?)</a>"#,
    )
    .unwrap_or_else(|_| regex::Regex::new(r#"(?i)never"$"#).unwrap());
    let mut results = Vec::new();
    for capture in pattern.captures_iter(html) {
        if results.len() >= max_results {
            break;
        }
        let link = capture.get(1).map(|m| m.as_str()).unwrap_or_default();
        let title = capture.get(2).map(|m| m.as_str()).unwrap_or_default();
        let snippet = capture.get(3).map(|m| m.as_str()).unwrap_or_default();
        results.push(format!("{}. {}\n{}\n{}", results.len() + 1, decode_html(title), decode_ddg_href(link), decode_html(snippet)));
    }
    results
}

pub fn clip_result(value: &str, limit: usize) -> String {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    if value.len() > limit {
        let mut cut = &value[..end];
        let _ = &mut cut;
        format!("{}\n[output truncated at {limit} bytes]", &value[..end])
    } else {
        value.to_string()
    }
}

const SEARCH_LIMIT: usize = 20_000;

pub async fn web_search(query: &str, max_results: usize) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("web_search requires a query");
    }
    let max_results = max_results.clamp(1, 20);
    if let Ok(brave_key) = std::env::var("BRAVE_SEARCH_API_KEY") {
        if !brave_key.is_empty() {
            let url = format!("https://api.search.brave.com/res/v1/web/search?q={}&count={}", urlencode(query), max_results);
            let response = http_client(30_000).get(&url).header("accept", "application/json").header("x-subscription-token", brave_key).send().await?;
            let status = response.status();
            if !status.is_success() {
                bail!("Brave search failed: HTTP {}", status.as_u16());
            }
            let body: Value = response.json().await?;
            let lines: Vec<String> = body["web"]["results"]
                .as_array()
                .map(|results| {
                    results
                        .iter()
                        .take(max_results)
                        .enumerate()
                        .map(|(index, result)| format!("{}. {}\n{}\n{}", index + 1, result["title"].as_str().unwrap_or_default(), result["url"].as_str().unwrap_or_default(), result["description"].as_str().unwrap_or_default()))
                        .collect()
                })
                .unwrap_or_default();
            return Ok(clip_result(&lines.join("\n"), SEARCH_LIMIT));
        }
    }
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    let response = http_client(30_000).get(&url).header("user-agent", "hl-harness/0.1").send().await?;
    let status = response.status();
    if !status.is_success() {
        bail!("DuckDuckGo search failed: HTTP {}", status.as_u16());
    }
    let html = response.text().await?;
    let results = parse_ddg_results(&html, max_results);
    Ok(clip_result(&if results.is_empty() { "No results".to_string() } else { results.join("\n") }, SEARCH_LIMIT))
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub async fn web_fetch(url: &str, max_bytes: usize) -> Result<String> {
    let parsed = url::Url::parse(url).map_err(|error| anyhow!("{error}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        bail!("web_fetch only supports http(s) URLs");
    }
    let host = parsed.host_str().unwrap_or_default();
    if is_private_host(host) {
        bail!("web_fetch blocks private and local network hosts");
    }
    let max_bytes = max_bytes.max(1);
    let response = http_client(30_000)
        .get(parsed)
        .header("accept", "text/plain, text/html, application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        bail!("Fetch failed: HTTP {}", status.as_u16());
    }
    let text = response.text().await?;
    let mut end = (max_bytes).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(text[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_host_blocking_matches_typescript() {
        for host in ["localhost", "sub.localhost", "host.local", "x.internal", "127.0.0.1", "10.1.2.3", "192.168.0.5", "172.16.9.1", "172.31.255.1", "169.254.1.1", "100.100.100.100", "0.0.0.0", "[::1]"] {
            assert!(is_private_host(host), "expected private: {host}");
        }
        for host in ["example.com", "8.8.8.8", "172.32.0.1", "100.200.1.2", "12.34.56.78"] {
            assert!(!is_private_host(host), "expected public: {host}");
        }
        assert!(is_private_host("::ffff:127.0.0.1"));
    }

    #[test]
    fn ddg_parsing_and_links() {
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2Fx&amp;rut=s1">Rust <b>Lang</b></a><a class="result__snippet">Systems lang &amp; more</a><a class="result__a" href="https://two.test/">Two</a>"#;
        let results = parse_ddg_results(html, 5);
        assert_eq!(results.len(), 1); // second anchor lacks a snippet pair
        assert!(results[0].contains("https://rust-lang.org/x"));
        assert!(results[0].contains("Rust Lang"));
    }
}
