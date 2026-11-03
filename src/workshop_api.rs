use std::collections::HashMap;
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::util::html_unescape;

const API_BASE: &str = "https://api.steampowered.com";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const SHAREDFILE_RE: &str = r"(?i)sharedfiles/filedetails(?:/)?\??[^\s]*?\bid=(\d+)";
const WORKSHOP_FILE_RE: &str = r"(?i)steamcommunity\.com/workshop/filedetails/\?id=(\d+)";
const APP_WORKSHOP_RE: &str = r"(?i)steamcommunity\.com/app/(\d+)(?:/.*)?/workshop/?";
const BARE_ID_RE: &str = r"^\s*(\d{6,18})\s*$";

fn regex(pattern: &'static str) -> &'static Regex {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some(existing) = map.get(pattern) {
        return existing;
    }
    let compiled: &'static Regex = Box::leak(Box::new(Regex::new(pattern).unwrap()));
    map.insert(pattern, compiled);
    compiled
}

/// Return the workshop file id in arbitrary text, if there is one.
pub fn extract_workshop_id(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(caps) = regex(SHAREDFILE_RE).captures(text) {
        return caps.get(1)?.as_str().parse().ok();
    }
    if let Some(caps) = regex(WORKSHOP_FILE_RE).captures(text) {
        return caps.get(1)?.as_str().parse().ok();
    }
    if text.contains("/app/") || text.contains("appid=") {
        return None;
    }
    regex(BARE_ID_RE)
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Return an app id from a URL or a bare number.
pub fn extract_app_id(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(caps) = regex(APP_WORKSHOP_RE).captures(text) {
        return caps.get(1)?.as_str().parse().ok();
    }
    if text.contains("steamcommunity.com") {
        if let Some(caps) = regex(r"(?i)(?:appid|appId)=(\d+)").captures(text) {
            return caps.get(1)?.as_str().parse().ok();
        }
    }
    regex(BARE_ID_RE)
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

pub fn item_appid(item: &Value) -> Option<u64> {
    for key in ["consumer_app_id", "creator_app_id", "appid"] {
        if let Some(value) = item.get(key).and_then(as_u64) {
            if value > 0 {
                return Some(value);
            }
        }
    }
    if let Some(consumers) = item.get("consumers").and_then(Value::as_array) {
        for consumer in consumers {
            if let Some(value) = consumer.get("appid").and_then(as_u64) {
                if value > 0 {
                    return Some(value);
                }
            } else if let Some(value) = consumer.as_str().and_then(|text| text.parse::<u64>().ok())
            {
                if value > 0 {
                    return Some(value);
                }
            }
        }
    }
    None
}

pub fn item_result_code(item: &Value) -> i64 {
    item.get("result").and_then(as_i64).unwrap_or(1)
}

pub fn item_error_message(item: &Value) -> Option<String> {
    match item_result_code(item) {
        1 => None,
        9 => Some("Item not found or no access".to_string()),
        15 => Some("Item is private".to_string()),
        16 => Some("Item is deleted".to_string()),
        17 => Some("Item is banned".to_string()),
        code => Some(format!("Steam returned result code {code}")),
    }
}

fn as_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_i64() {
        return (number > 0).then_some(number as u64);
    }
    value.as_str().and_then(|text| text.parse::<u64>().ok())
}

fn as_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_u64() {
        return i64::try_from(number).ok();
    }
    value.as_str().and_then(|text| text.parse::<i64>().ok())
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(30))
            .user_agent(BROWSER_UA)
            .build()
    })
}

fn post_form(url: &str, fields: &[(String, String)]) -> Result<Value, String> {
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    match agent().post(url).send_form(&pairs) {
        Ok(response) => {
            let mut text = String::new();
            response
                .into_reader()
                .take(8 * 1024 * 1024)
                .read_to_string(&mut text)
                .map_err(|error| format!("Steam API read failed: {error}"))?;
            serde_json::from_str(&text)
                .map_err(|error| format!("Steam API returned invalid JSON: {error}"))
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("Steam API HTTP {code}")),
        Err(error) => Err(format!("Steam API request failed: {error}")),
    }
}

pub struct CollectionInfo {
    pub is_collection: bool,
    pub children: Vec<u64>,
    pub error: Option<String>,
}

pub fn get_collection_details(fileid: u64) -> CollectionInfo {
    let fields = vec![
        ("collectioncount".to_string(), "1".to_string()),
        ("publishedfileids[0]".to_string(), fileid.to_string()),
        ("format".to_string(), "json".to_string()),
    ];
    let url = format!("{API_BASE}/ISteamRemoteStorage/GetCollectionDetails/v1/");
    let Ok(payload) = post_form(&url, &fields) else {
        return CollectionInfo {
            is_collection: false,
            children: Vec::new(),
            error: Some("Could not reach the Steam API".to_string()),
        };
    };
    let Some(collections) = payload
        .get("response")
        .and_then(|response| response.get("collectiondetails"))
        .and_then(Value::as_array)
    else {
        return CollectionInfo {
            is_collection: false,
            children: Vec::new(),
            error: Some("Unexpected collection response".to_string()),
        };
    };
    let Some(entry) = collections.first() else {
        return CollectionInfo {
            is_collection: false,
            children: Vec::new(),
            error: None,
        };
    };
    if entry.get("result").and_then(as_i64).unwrap_or(1) != 1 {
        return CollectionInfo {
            is_collection: false,
            children: Vec::new(),
            error: None,
        };
    }
    let children = entry
        .get("children")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|child| child.get("publishedfileid").and_then(as_u64))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    CollectionInfo {
        is_collection: !children.is_empty() || entry.get("children").is_some(),
        children,
        error: None,
    }
}

pub fn get_published_file_details(fileids: &[u64]) -> Result<Vec<Value>, String> {
    if fileids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = fileids.iter().take(100).map(u64::to_string).collect();
    let mut fields = vec![
        ("itemcount".to_string(), ids.len().to_string()),
        ("format".to_string(), "json".to_string()),
    ];
    for (index, fileid) in ids.iter().enumerate() {
        fields.push((format!("publishedfileids[{index}]"), fileid.clone()));
    }
    let url = format!("{API_BASE}/ISteamRemoteStorage/GetPublishedFileDetails/v1/");
    let payload = post_form(&url, &fields)?;
    payload
        .get("response")
        .and_then(|response| response.get("publishedfiledetails"))
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "Unexpected details response".to_string())
}

pub fn clean_description(text: &str) -> String {
    static HSPACE: OnceLock<Regex> = OnceLock::new();
    static LINE: OnceLock<Regex> = OnceLock::new();
    static BLANK: OnceLock<Regex> = OnceLock::new();
    let hspace = HSPACE.get_or_init(|| Regex::new(r"[ \t]+").unwrap());
    let line = LINE.get_or_init(|| Regex::new(r"[ \t]*\r?\n[ \t]*").unwrap());
    let blank = BLANK.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let text = hspace.replace_all(text, " ");
    let text = line.replace_all(&text, "\n");
    let text = blank.replace_all(&text, "\n\n");
    text.trim().to_string()
}

fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in bytes.iter().enumerate().skip(start) {
        match (*byte, quote) {
            (b'"', None) | (b'\'', None) => quote = Some(*byte),
            (b'"', Some(current)) if current == b'"' => quote = None,
            (b'\'', Some(current)) if current == b'\'' => quote = None,
            (b'>', None) => return Some(offset),
            _ => {}
        }
    }
    None
}

fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "li"
            | "ul"
            | "ol"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "tr"
            | "td"
            | "table"
            | "pre"
    )
}

/// Extract the inner HTML of a div, keeping nested divs intact.
fn extract_div_inner(page: &str, content_start: usize) -> String {
    let lower = page.to_ascii_lowercase();
    let bytes = page.as_bytes();
    let mut out = String::new();
    let mut depth = 1usize;
    let mut i = content_start;

    while i < page.len() {
        let Some(relative) = page[i..].find('<') else {
            out.push_str(&page[i..]);
            break;
        };
        let lt = i + relative;
        out.push_str(&page[i..lt]);

        if page[lt..].starts_with("<!--") {
            if let Some(end) = page[lt + 4..].find("-->") {
                i = lt + 4 + end + 3;
                continue;
            }
            break;
        }

        let Some(gt) = find_tag_end(bytes, lt) else {
            break;
        };
        let tag = lower[lt + 1..gt].trim();

        if let Some(rest) = tag.strip_prefix('/') {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("");
            if name == "div" {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
                out.push('\n');
            } else if is_block_tag(name) {
                out.push('\n');
            }
        } else if tag.starts_with('!') || tag.starts_with('?') {
            // doctype / processing instruction / comment tail
        } else {
            let name = tag
                .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .next()
                .unwrap_or("");

            if name == "script" || name == "style" {
                let closing = format!("</{name}");
                if let Some(end) = lower[gt + 1..].find(&closing) {
                    let close_start = gt + 1 + end;
                    if let Some(close_end) = find_tag_end(bytes, close_start) {
                        i = close_end + 1;
                        continue;
                    }
                }
                break;
            }

            if name == "div" {
                depth += 1;
                out.push('\n');
            } else if name == "br" || name == "li" || is_block_tag(name) {
                out.push('\n');
            }
        }

        i = gt + 1;
    }

    out
}

fn find_description_div(page: &str) -> Option<(usize, usize)> {
    let lower = page.to_ascii_lowercase();
    let bytes = page.as_bytes();
    let mut from = 0usize;
    loop {
        let relative = lower[from..].find("<div")?;
        let open = from + relative;
        let open_end = find_tag_end(bytes, open)?;
        let tag = &page[open..=open_end];
        let is_description = attr_value(tag, "class")
            .map(|class| {
                class
                    .split_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("workshopItemDescription"))
            })
            .unwrap_or(false);
        if is_description {
            return Some((open, open_end));
        }
        from = open_end + 1;
    }
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = attr.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(relative) = lower[from..].find(&needle) {
        let start = from + relative;
        let before_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let after = start + needle.len();
        let mut cursor = after;
        while lower
            .as_bytes()
            .get(cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            cursor += 1;
        }
        if before_ok && lower.as_bytes().get(cursor) == Some(&b'=') {
            cursor += 1;
            while lower
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                cursor += 1;
            }
            if let Some(quote) = lower
                .as_bytes()
                .get(cursor)
                .copied()
                .filter(|b| *b == b'"' || *b == b'\'')
            {
                cursor += 1;
                if let Some(end) = tag[cursor..].find(quote as char) {
                    return Some(html_unescape(&tag[cursor..cursor + end]));
                }
            }
        }
        from = start + needle.len();
    }
    None
}

fn find_meta_description(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let bytes = page.as_bytes();
    let mut from = 0usize;
    while let Some(relative) = lower[from..].find("<meta") {
        let start = from + relative;
        let Some(end) = find_tag_end(bytes, start) else {
            break;
        };
        let tag = &page[start..=end];
        let property = attr_value(tag, "property")
            .or_else(|| attr_value(tag, "name"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if property == "og:description" || property == "description" {
            if let Some(content) = attr_value(tag, "content") {
                let content = clean_description(&content);
                if !content.is_empty() {
                    return Some(content);
                }
            }
        }
        from = end + 1;
    }
    None
}

pub fn parse_item_description(page: &str) -> String {
    if page.is_empty() {
        return String::new();
    }
    if let Some((open, open_end)) = find_description_div(page) {
        let _ = open;
        let raw = extract_div_inner(page, open_end + 1);
        let text = clean_description(&html_unescape(&raw));
        if !text.is_empty() {
            return text;
        }
    }
    find_meta_description(page).unwrap_or_default()
}

use std::process::Command;

fn fetch_page(fileid: u64, timeout_secs: u64) -> String {
    let url = format!("https://steamcommunity.com/sharedfiles/filedetails/?id={fileid}&l=english");
    // Steam's edge serves rustls/ureq requests a 429 while curl succeeds, so use
    // curl when it is available and fall back to the built-in client.
    if let Some(page) = fetch_page_curl(&url, timeout_secs) {
        if !page.trim().is_empty() {
            return page;
        }
    }
    for attempt in 0..2 {
        let result = agent()
            .get(&url)
            .timeout(Duration::from_secs(timeout_secs))
            .set(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .set("Accept-Language", "en-US,en;q=0.9")
            .call();
        match result {
            Ok(response) => {
                let mut bytes = Vec::new();
                if response
                    .into_reader()
                    .take(12 * 1024 * 1024)
                    .read_to_end(&mut bytes)
                    .is_ok()
                {
                    return String::from_utf8_lossy(&bytes).into_owned();
                }
                return String::new();
            }
            Err(ureq::Error::Status(429, _)) if attempt == 0 => {
                std::thread::sleep(Duration::from_millis(1600));
            }
            Err(_) => break,
        }
    }
    String::new()
}

fn fetch_page_curl(url: &str, timeout_secs: u64) -> Option<String> {
    let output = Command::new("curl")
        .args([
            "-sSL",
            "--compressed",
            "--fail",
            "--max-time",
            &timeout_secs.to_string(),
            "-A",
            BROWSER_UA,
            "-H",
            "Accept-Language: en-US,en;q=0.9",
            url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn description_cache() -> &'static Mutex<HashMap<u64, String>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn get_item_description(fileid: u64) -> String {
    if fileid == 0 {
        return String::new();
    }
    if let Some(cached) = description_cache().lock().unwrap().get(&fileid) {
        if !cached.is_empty() {
            return cached.clone();
        }
    }
    let description = parse_item_description(&fetch_page(fileid, 15));
    if !description.is_empty() {
        description_cache()
            .lock()
            .unwrap()
            .insert(fileid, description.clone());
    }
    description
}

pub fn fetch_preview_bytes(url: &str) -> Result<Vec<u8>, String> {
    if url.trim().is_empty() {
        return Err("No preview URL".to_string());
    }
    let response = agent()
        .get(url)
        .timeout(Duration::from_secs(25))
        .set("User-Agent", BROWSER_UA)
        .set("Referer", "https://steamcommunity.com/")
        .call()
        .map_err(|error| format!("Preview request failed: {error}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(30 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Preview read failed: {error}"))?;
    if bytes.is_empty() {
        return Err("Preview response was empty".to_string());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ids() {
        assert_eq!(
            extract_workshop_id(
                "https://steamcommunity.com/sharedfiles/filedetails/?id=2858447110"
            ),
            Some(2858447110)
        );
        assert_eq!(
            extract_workshop_id(
                "https://steamcommunity.com/sharedfiles/filedetails/?id=123456&x=1"
            ),
            Some(123456)
        );
        assert_eq!(extract_workshop_id("2858447110"), Some(2858447110));
        assert_eq!(extract_workshop_id("hello"), None);
        assert_eq!(
            extract_app_id("https://steamcommunity.com/app/431960/workshop/"),
            Some(431960)
        );
    }

    #[test]
    fn parses_nested_description() {
        let page = r#"
            <div class="workshopItemDescriptionTitle">Description</div>
            <div class="workshopItemDescription" id="highlightContent">
              <div class="bb_h1">Hello</div>
              <p>Line <b>one</b><br>Line two &amp; more</p>
              <div><div>nested</div></div>
            </div>
            <div>after</div>
        "#;
        let desc = parse_item_description(page);
        assert_ne!(desc, "Description");
        assert!(desc.contains("Hello"));
        assert!(desc.contains("Line one"));
        assert!(desc.contains("Line two & more"));
        assert!(desc.contains("nested"));
        assert!(!desc.contains("after"));
    }

    #[test]
    fn falls_back_to_meta_description() {
        let page = r#"
            <html><head>
              <meta property="og:description" content="A short &amp; useful summary.">
            </head><body>no description div</body></html>
        "#;
        assert_eq!(parse_item_description(page), "A short & useful summary.");
    }

    #[test]
    fn empty_description_stays_empty() {
        let page = r#"
            <div class="workshopItemDescription" id="highlightContent"></div>
            <meta property="og:description" content="">
        "#;
        assert!(parse_item_description(page).is_empty());
    }

    #[test]
    #[ignore = "fetches live Steam Community pages"]
    fn live_descriptions() {
        let direct = get_item_description(3863);
        assert!(!direct.is_empty(), "expected a description for item 3863");
        assert!(!direct.contains('<'), "description still contains markup");
        assert!(
            direct.contains("UPDATE"),
            "unexpected description: {direct}"
        );

        let short = get_item_description(3341062649);
        assert!(short.contains("konata"), "unexpected description: {short}");

        let empty = get_item_description(2858447110);
        assert!(empty.is_empty(), "expected an empty description: {empty}");
    }
}
