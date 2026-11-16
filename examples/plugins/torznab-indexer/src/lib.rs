//! A Torznab indexer as a cellarr plugin.
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown`, then drop
//! `target/wasm32-unknown-unknown/release/cellarr_plugin_torznab.wasm` into
//! cellarr's `plugins/` directory. It shows up as the `TorznabPlugin` indexer
//! implementation, configured with the fields below.

wit_bindgen::generate!({
    path: "../../../crates/cellarr-plugins/wit/plugins.wit",
    world: "indexer-plugin",
});

use cellarr::plugins::http::{send, HttpRequest};
use cellarr::plugins::types::{Field, PluginError, PluginInfo, Protocol, Release, SearchTerms};
use exports::cellarr::plugins::{indexer, plugin};

struct Torznab;

fn err(message: impl Into<String>) -> PluginError {
    PluginError {
        message: message.into(),
    }
}

fn field(name: &str, label: &str, kind: &str, default: &str, privacy: bool) -> Field {
    Field {
        name: name.into(),
        label: label.into(),
        kind: kind.into(),
        default_value: default.into(),
        help_text: String::new(),
        advanced: false,
        privacy,
    }
}

struct Settings {
    base: String,
    api_path: String,
    api_key: String,
    categories: Vec<String>,
}

fn settings(json: &str) -> Result<Settings, PluginError> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| err(e.to_string()))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let base = s("baseUrl").trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err(err("baseUrl is required"));
    }
    let api_path = match s("apiPath") {
        p if p.is_empty() => "/api".to_string(),
        p => p,
    };
    let categories = v
        .get("categories")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().map(|c| c.to_string().trim_matches('"').to_string()).collect())
        .unwrap_or_default();
    Ok(Settings {
        base,
        api_path,
        api_key: s("apiKey"),
        categories,
    })
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn get(s: &Settings, params: &[(&str, String)]) -> Result<String, PluginError> {
    let mut url = format!("{}{}?", s.base, s.api_path);
    let mut first = true;
    for (k, v) in params {
        if !first {
            url.push('&');
        }
        first = false;
        url.push_str(&format!("{k}={}", encode(v)));
    }
    if !s.api_key.is_empty() {
        url.push_str(&format!("&apikey={}", encode(&s.api_key)));
    }
    let resp = send(&HttpRequest {
        method: "GET".into(),
        url,
        headers: vec![("Accept".into(), "application/rss+xml, application/xml".into())],
        body: Vec::new(),
    })?;
    if resp.status != 200 {
        return Err(err(format!("indexer answered HTTP {}", resp.status)));
    }
    String::from_utf8(resp.body).map_err(|e| err(e.to_string()))
}

/// The text between `<tag ...>` and `</tag>`, unescaped.
fn element<'a>(xml: &'a str, tag: &str) -> Option<String> {
    let open = xml.find(&format!("<{tag}"))?;
    let start = open + xml[open..].find('>')? + 1;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(unescape(xml[start..end].trim()))
}

/// The value of `attr="..."` on the first `<tag .../>`.
fn attribute(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = xml.find(&format!("<{tag}"))?;
    let close = open + xml[open..].find('>')?;
    let head = &xml[open..close];
    let key = format!("{attr}=\"");
    let at = head.find(&key)? + key.len();
    let end = at + head[at..].find('"')?;
    Some(unescape(&head[at..end]))
}

/// `<torznab:attr name="NAME" value="..."/>`.
fn torznab_attr(item: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\"");
    let at = item.find(&needle)?;
    let rest = &item[at..];
    let v = rest.find("value=\"")? + 7;
    let end = v + rest[v..].find('"')?;
    Some(unescape(&rest[v..end]))
}

fn unescape(s: &str) -> String {
    let s = s.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")).unwrap_or(s);
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn parse(xml: &str) -> Vec<Release> {
    let mut out = Vec::new();
    for chunk in xml.split("<item>").skip(1) {
        let item = chunk.split("</item>").next().unwrap_or_default();
        let Some(title) = element(item, "title") else { continue };
        let link = attribute(item, "enclosure", "url")
            .or_else(|| element(item, "link"))
            .unwrap_or_default();
        if link.is_empty() {
            continue;
        }
        let size = torznab_attr(item, "size")
            .or_else(|| attribute(item, "enclosure", "length"))
            .and_then(|s| s.parse().ok());
        let mut flags = Vec::new();
        if torznab_attr(item, "downloadvolumefactor").as_deref() == Some("0") {
            flags.push("freeleech".to_string());
        }
        out.push(Release {
            title,
            download_url: link,
            guid: element(item, "guid"),
            protocol: Protocol::Torrent,
            size,
            seeders: torznab_attr(item, "seeders").and_then(|s| s.parse().ok()),
            indexer_flags: flags,
        });
    }
    out
}

impl plugin::Guest for Torznab {
    fn info() -> PluginInfo {
        PluginInfo {
            name: "TorznabPlugin".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Torznab indexer (example plugin)".into(),
            protocol: Protocol::Torrent,
        }
    }

    fn fields() -> Vec<Field> {
        vec![
            field("baseUrl", "URL", "textbox", "\"\"", false),
            field("apiPath", "API Path", "textbox", "\"/api\"", false),
            field("apiKey", "API Key", "textbox", "\"\"", true),
            field("categories", "Categories", "select", "[3000,3010,3030,3040]", false),
        ]
    }
}

impl indexer::Guest for Torznab {
    fn search(settings_json: String, terms: SearchTerms) -> Result<Vec<Release>, PluginError> {
        let s = settings(&settings_json)?;
        let lookup = |k: &str| {
            terms
                .ids
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let mut params: Vec<(&str, String)> = Vec::new();
        match (lookup("artist"), lookup("album")) {
            (Some(artist), album) => {
                params.push(("t", "music".into()));
                params.push(("artist", artist));
                if let Some(album) = album {
                    params.push(("album", album));
                }
            }
            _ => {
                params.push(("t", "search".into()));
                params.push(("q", terms.queries.first().cloned().unwrap_or_default()));
            }
        }
        if !s.categories.is_empty() {
            params.push(("cat", s.categories.join(",")));
        }
        Ok(parse(&get(&s, &params)?))
    }

    fn latest(settings_json: String) -> Result<Vec<Release>, PluginError> {
        let s = settings(&settings_json)?;
        let mut params = vec![("t", "search".to_string())];
        if !s.categories.is_empty() {
            params.push(("cat", s.categories.join(",")));
        }
        Ok(parse(&get(&s, &params)?))
    }

    fn test(settings_json: String) -> Result<(), PluginError> {
        let s = settings(&settings_json)?;
        let caps = get(&s, &[("t", "caps".into())])?;
        if caps.contains("<caps") {
            Ok(())
        } else {
            Err(err("the URL did not answer a Torznab capabilities request"))
        }
    }
}

export!(Torznab);
