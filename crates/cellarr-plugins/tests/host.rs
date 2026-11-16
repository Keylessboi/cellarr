//! The plugin host against real plugins: the two examples under
//! `examples/plugins/`, built for `wasm32-unknown-unknown` and committed as
//! fixtures (`just plugin-examples` rebuilds them). Their HTTP goes to an
//! in-test transport, so nothing leaves the process.

#![cfg(feature = "wasm")]

use std::sync::{Arc, Mutex};

use cellarr_plugins::capability::{HttpRequest, HttpResponse};
use cellarr_plugins::host::{PluginDownloadState, PluginSearch, Transport};
use cellarr_plugins::{HostConfig, Plugin, PluginError, PluginKind, PluginProtocol};
use serde_json::json;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{DIR}/{name}")).unwrap()
}

fn config() -> HostConfig {
    HostConfig::default()
        .with_http(true)
        .with_max_instances(8)
        .with_fuel(Some(5_000_000_000))
        .with_max_memory_bytes(64 << 20)
}

/// A transport that records every request and answers with `reply`.
fn recording(
    reply: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static,
) -> (Transport, Arc<Mutex<Vec<HttpRequest>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let t: Transport = Arc::new(move |r: &HttpRequest| {
        log.lock().unwrap().push(r.clone());
        Ok(reply(r))
    });
    (t, seen)
}

fn ok(body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    }
}

const RSS: &str = r#"<?xml version="1.0"?><rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>
<item><title>Radiohead - Kid A (2000) [FLAC]</title><guid>g1</guid>
<enclosure url="magnet:?xt=urn:btih:abcdef0123456789abcdef0123456789abcdef01&amp;dn=Kid+A" length="300000000"/>
<torznab:attr name="seeders" value="25"/><torznab:attr name="downloadvolumefactor" value="0"/></item>
<item><title>No link here</title></item>
</channel></rss>"#;

#[test]
fn a_built_indexer_plugin_loads_describes_itself_and_searches() {
    let (t, seen) = recording(|_| ok(RSS));
    let p = Plugin::from_bytes(&fixture("torznab.wasm"), config())
        .expect("the example indexer loads from a plain core module")
        .with_transport(t);
    assert_eq!(p.kind(), PluginKind::Indexer);
    assert_eq!(p.info().name, "TorznabPlugin");
    assert_eq!(p.info().protocol, PluginProtocol::Torrent);
    let names: Vec<_> = p.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["baseUrl", "apiPath", "apiKey", "categories"]);
    assert!(p.fields()[2].privacy, "the API key is marked secret");

    let settings =
        json!({"baseUrl": "http://prowlarr.test:9696/1", "apiKey": "k", "categories": [3040]});
    let search = PluginSearch {
        queries: vec!["Radiohead Kid A".into()],
        ids: vec![
            ("artist".into(), "Radiohead".into()),
            ("album".into(), "Kid A".into()),
        ],
        numbering: Vec::new(),
    };
    let found = p.search(&settings, &search).unwrap();
    assert_eq!(found.len(), 1, "the item without a link is dropped");
    assert_eq!(found[0].title, "Radiohead - Kid A (2000) [FLAC]");
    assert!(found[0]
        .download_url
        .starts_with("magnet:?xt=urn:btih:abcdef"));
    assert_eq!(found[0].seeders, Some(25));
    assert_eq!(found[0].size, Some(300_000_000));
    assert_eq!(found[0].indexer_flags, ["freeleech"]);

    let url = seen.lock().unwrap()[0].url.clone();
    assert!(
        url.starts_with("http://prowlarr.test:9696/1/api?t=music&artist=Radiohead&album=Kid%20A"),
        "{url}"
    );
    assert!(
        url.contains("cat=3040") && url.contains("apikey=k"),
        "{url}"
    );
}

#[test]
fn a_plugin_can_only_reach_the_hosts_its_settings_name() {
    let (t, seen) = recording(|_| ok(RSS));
    let p = Plugin::from_bytes(&fixture("torznab.wasm"), config())
        .unwrap()
        .with_transport(t);
    // Allowed: the configured host.
    p.latest(&json!({"baseUrl": "http://indexer.test"}))
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
    // With no HTTP granted at all, nothing goes out.
    let denied = Plugin::from_bytes(&fixture("torznab.wasm"), config().with_http(false))
        .unwrap()
        .with_transport(recording(|_| ok(RSS)).0);
    let e = denied
        .latest(&json!({"baseUrl": "http://indexer.test"}))
        .unwrap_err();
    assert!(
        matches!(e, PluginError::Guest(ref m) if m.contains("not granted")),
        "{e:?}"
    );
}

#[test]
fn a_built_download_client_plugin_drives_a_qbittorrent_api() {
    let (t, seen) = recording(|r| {
        if r.url.ends_with("/auth/login") {
            HttpResponse {
                status: 200,
                headers: vec![("set-cookie".into(), "SID=abc; HttpOnly".into())],
                body: b"Ok.".to_vec(),
            }
        } else if r.url.contains("/torrents/info") {
            ok(
                r#"[{"state":"pausedUP","progress":1.0,"ratio":1.5,"seeding_time":60,
                   "num_seeds":3,"num_leechs":1,"content_path":"/downloads/Kid A"}]"#,
            )
        } else {
            ok("Ok.")
        }
    });
    let p = Plugin::from_bytes(&fixture("qbit.wasm"), config())
        .unwrap()
        .with_transport(t);
    assert_eq!(p.kind(), PluginKind::DownloadClient);
    assert_eq!(p.info().name, "QBittorrentPlugin");
    let settings = json!({"host": "qbit.test", "port": 8080, "username": "u", "password": "p"});
    let id = p
        .add(
            &settings,
            "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01",
            "lidarr",
        )
        .unwrap();
    assert_eq!(id, "abcdef0123456789abcdef0123456789abcdef01");
    let st = p.status(&settings, &id).unwrap();
    assert_eq!(st.state, PluginDownloadState::Completed);
    assert_eq!(st.content_path.as_deref(), Some("/downloads/Kid A"));
    assert_eq!(st.peers, Some(4));
    p.remove(&settings, &id, false).unwrap();

    let reqs = seen.lock().unwrap();
    let add = reqs
        .iter()
        .find(|r| r.url.ends_with("/torrents/add"))
        .unwrap();
    assert!(
        add.headers
            .iter()
            .any(|(k, v)| k == "Cookie" && v == "SID=abc"),
        "session cookie sent"
    );
    assert!(String::from_utf8_lossy(&add.body).contains("category=lidarr"));
    assert!(reqs
        .iter()
        .all(|r| r.url.starts_with("http://qbit.test:8080/")));
}

#[test]
fn a_plugin_that_runs_too_long_is_stopped() {
    let (t, _) = recording(|_| ok(&RSS.repeat(200)));
    let p = Plugin::from_bytes(
        &fixture("torznab.wasm"),
        config().with_fuel(Some(20_000_000)),
    )
    .unwrap()
    .with_transport(t);
    let e = p
        .latest(&json!({"baseUrl": "http://indexer.test"}))
        .unwrap_err();
    assert!(matches!(e, PluginError::ResourceLimit(_)), "{e:?}");
}

#[test]
fn garbage_and_foreign_components_are_refused() {
    assert!(matches!(
        Plugin::from_bytes(b"not wasm at all", config()),
        Err(PluginError::Load(_))
    ));
    assert!(matches!(
        Plugin::from_bytes(b"(component)", config()),
        Err(PluginError::Load(_))
    ));
}

#[test]
fn a_directory_of_plugins_loads_into_a_registry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        format!("{DIR}/torznab.wasm"),
        dir.path().join("torznab.wasm"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("qbit")).unwrap();
    std::fs::copy(
        format!("{DIR}/qbit.wasm"),
        dir.path().join("qbit/plugin.wasm"),
    )
    .unwrap();
    std::fs::write(dir.path().join("qbit/plugin.json"), r#"{"memoryMib": 32}"#).unwrap();
    std::fs::write(dir.path().join("broken.wasm"), b"nope").unwrap();
    let reg = cellarr_plugins::registry::Registry::load_dir(dir.path(), recording(|_| ok("")).0);
    assert!(
        reg.get("torznabplugin").is_some(),
        "names match case-insensitively"
    );
    assert_eq!(reg.of_kind(PluginKind::DownloadClient).len(), 1);
    assert_eq!(
        reg.failures().len(),
        1,
        "the broken one is reported, not fatal"
    );
}
