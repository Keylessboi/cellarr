//! A qBittorrent (WebUI API v2) download client as a cellarr plugin.
//!
//! It takes the same fields Lidarr's own qBittorrent client does, so a config
//! copied from Lidarr works unchanged. Each call logs in, does its one thing, and
//! is done: plugins keep no state between calls.

wit_bindgen::generate!({
    path: "../../../crates/cellarr-plugins/wit/plugins.wit",
    world: "download-client-plugin",
});

use cellarr::plugins::http::{send, HttpRequest, HttpResponse};
use cellarr::plugins::types::{Field, PluginError, PluginInfo, Protocol};
use exports::cellarr::plugins::download_client::{DownloadState, DownloadStatus, Guest};
use exports::cellarr::plugins::plugin;

struct Qbit;

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

struct Session {
    base: String,
    cookie: String,
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

fn request(method: &str, url: String, cookie: &str, body: Vec<u8>) -> Result<HttpResponse, PluginError> {
    let mut headers = vec![("Referer".to_string(), url.clone())];
    if !cookie.is_empty() {
        headers.push(("Cookie".into(), cookie.into()));
    }
    if !body.is_empty() {
        headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
    }
    send(&HttpRequest {
        method: method.into(),
        url,
        headers,
        body,
    })
}

fn login(settings_json: &str) -> Result<Session, PluginError> {
    let v: serde_json::Value = serde_json::from_str(settings_json).map_err(|e| err(e.to_string()))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let host = s("host");
    if host.is_empty() {
        return Err(err("host is required"));
    }
    let port = v.get("port").and_then(serde_json::Value::as_u64).unwrap_or(8080);
    let scheme = if v.get("useSsl").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        "https"
    } else {
        "http"
    };
    let base_path = s("urlBase");
    let base_path = base_path.trim_matches('/');
    let base = if base_path.is_empty() {
        format!("{scheme}://{host}:{port}")
    } else {
        format!("{scheme}://{host}:{port}/{base_path}")
    };
    let resp = request(
        "POST",
        format!("{base}/api/v2/auth/login"),
        "",
        form(&[("username", &s("username")), ("password", &s("password"))]),
    )?;
    if resp.status != 200 {
        return Err(err(format!("login refused: HTTP {}", resp.status)));
    }
    let cookie = resp
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(_, v)| v.split(';').next())
        .find(|c| c.contains("SID="))
        .unwrap_or_default()
        .to_string();
    Ok(Session { base, cookie })
}

fn infohash(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let at = lower.find("urn:btih:")? + 9;
    let hash: String = lower[at..].chars().take_while(char::is_ascii_alphanumeric).collect();
    (hash.len() == 40).then_some(hash)
}

impl plugin::Guest for Qbit {
    fn info() -> PluginInfo {
        PluginInfo {
            name: "QBittorrentPlugin".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description: "qBittorrent download client (example plugin)".into(),
            protocol: Protocol::Torrent,
        }
    }

    fn fields() -> Vec<Field> {
        vec![
            field("host", "Host", "textbox", "\"localhost\"", false),
            field("port", "Port", "number", "8080", false),
            field("useSsl", "Use SSL", "checkbox", "false", false),
            field("urlBase", "URL Base", "textbox", "\"\"", false),
            field("username", "Username", "textbox", "\"\"", false),
            field("password", "Password", "password", "\"\"", true),
            field("musicCategory", "Category", "textbox", "\"lidarr\"", false),
        ]
    }
}

impl Guest for Qbit {
    fn add(settings: String, url: String, category: String) -> Result<String, PluginError> {
        let hash = infohash(&url).ok_or_else(|| err("only magnet links carry an infohash"))?;
        let s = login(&settings)?;
        let resp = request(
            "POST",
            format!("{}/api/v2/torrents/add", s.base),
            &s.cookie,
            form(&[("urls", &url), ("category", &category)]),
        )?;
        if resp.status != 200 {
            return Err(err(format!("add refused: HTTP {}", resp.status)));
        }
        Ok(hash)
    }

    fn status(settings: String, id: String) -> Result<DownloadStatus, PluginError> {
        let s = login(&settings)?;
        let resp = request(
            "GET",
            format!("{}/api/v2/torrents/info?hashes={}", s.base, encode(&id)),
            &s.cookie,
            Vec::new(),
        )?;
        let rows: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|e| err(e.to_string()))?;
        let Some(t) = rows.as_array().and_then(|a| a.first()) else {
            return Err(err(format!("no torrent {id}")));
        };
        let state_name = t.get("state").and_then(|x| x.as_str()).unwrap_or_default();
        let progress = t.get("progress").and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32;
        let state = match state_name {
            "error" | "missingFiles" => DownloadState::Failed,
            "uploading" | "stalledUP" | "pausedUP" | "stoppedUP" | "queuedUP" | "forcedUP"
            | "checkingUP" => DownloadState::Completed,
            "queuedDL" | "metaDL" | "allocating" => DownloadState::Queued,
            _ if progress >= 1.0 => DownloadState::Completed,
            _ => DownloadState::Downloading,
        };
        let num = |k: &str| t.get(k).and_then(serde_json::Value::as_u64);
        Ok(DownloadStatus {
            state,
            progress,
            content_path: (state == DownloadState::Completed)
                .then(|| t.get("content_path").and_then(|x| x.as_str()).map(str::to_string))
                .flatten(),
            ratio: t.get("ratio").and_then(serde_json::Value::as_f64).map(|r| r as f32),
            seeding_time_secs: num("seeding_time"),
            peers: Some((num("num_seeds").unwrap_or(0) + num("num_leechs").unwrap_or(0)) as u32),
            error_message: (state == DownloadState::Failed).then(|| state_name.to_string()),
        })
    }

    fn remove(settings: String, id: String, delete_data: bool) -> Result<(), PluginError> {
        let s = login(&settings)?;
        let del = if delete_data { "true" } else { "false" };
        let resp = request(
            "POST",
            format!("{}/api/v2/torrents/delete", s.base),
            &s.cookie,
            form(&[("hashes", &id), ("deleteFiles", del)]),
        )?;
        if resp.status == 200 {
            Ok(())
        } else {
            Err(err(format!("delete refused: HTTP {}", resp.status)))
        }
    }

    fn test(settings: String) -> Result<(), PluginError> {
        login(&settings).map(|_| ())
    }
}

export!(Qbit);
