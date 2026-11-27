//! The Test buttons for native Soulseek and Apple Music rows really exercise the
//! row: a wrong key, a bad URL, an unhealthy decryptor or `native: false` fail
//! with a Lidarr-style validation body, and secrets never appear in it.

mod lidarr_common;

use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use cellarr_api::AuthConfig;
use lidarr_common::start;
use serde_json::{json, Value};

const SLSKD_KEY: &str = "slskd-secret-key-123";
const DEV_TOKEN: &str = "apple-dev-token-secret-456";
const USER_TOKEN: &str = "apple-user-token-secret-789";

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

async fn fake_slskd() -> String {
    serve(Router::new().route(
        "/api/v0/application",
        get(|h: HeaderMap| async move {
            if h.get("x-api-key").and_then(|v| v.to_str().ok()) == Some(SLSKD_KEY) {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        }),
    ))
    .await
}

async fn fake_apple(healthy: bool) -> (String, String) {
    let api = serve(Router::new().route(
        "/v1/catalog/{sf}/search",
        get(|h: HeaderMap| async move {
            let ok = h
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.ends_with(DEV_TOKEN));
            if ok {
                (StatusCode::OK, r#"{"results":{}}"#)
            } else {
                (StatusCode::UNAUTHORIZED, "")
            }
        }),
    ))
    .await;
    let dec = serve(Router::new().route(
        "/healthz",
        get(move || async move {
            if healthy {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    ))
    .await;
    (format!("{api}/v1/"), dec)
}

fn fields(pairs: &[(&str, Value)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .map(|(n, v)| json!({"name": n, "value": v}))
            .collect(),
    )
}

/// POST a test body; returns (status, body text).
async fn test(path: &str, implementation: &str, f: Value) -> (u16, String) {
    let l = start(AuthConfig::disabled()).await;
    let r = l
        .req(reqwest::Method::POST, path.trim_start_matches("/api/v1"))
        .json(&json!({"name": "n", "implementation": implementation, "fields": f}))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn assert_failure(status: u16, body: &str, property: &str) {
    assert_eq!(status, 400, "{body}");
    let v: Value = serde_json::from_str(body).unwrap();
    assert_eq!(v[0]["propertyName"], property, "{body}");
    assert_eq!(v[0]["severity"], "error");
    assert!(v[0]["errorMessage"].as_str().is_some_and(|m| !m.is_empty()));
    for secret in [SLSKD_KEY, DEV_TOKEN, USER_TOKEN, "wrong-key"] {
        assert!(!body.contains(secret), "secret leaked: {body}");
    }
}

#[tokio::test]
async fn slskd_rows_are_really_tested_on_both_endpoints() {
    let url = fake_slskd().await;
    for (path, imp) in [
        ("/api/v1/indexer/test", "SlskdIndexer"),
        ("/api/v1/downloadclient/test", "SlskdClient"),
    ] {
        let (st, body) = test(
            path,
            imp,
            fields(&[("baseUrl", json!(url)), ("apiKey", json!(SLSKD_KEY))]),
        )
        .await;
        assert_eq!(st, 200, "{imp}: {body}");
        assert!(body.contains("isValid"), "{body}");

        let (st, body) = test(
            path,
            imp,
            fields(&[("baseUrl", json!(url)), ("apiKey", json!("wrong-key"))]),
        )
        .await;
        assert_failure(st, &body, "apiKey");

        let (st, body) = test(
            path,
            imp,
            fields(&[
                ("baseUrl", json!("http://127.0.0.1:1")),
                ("apiKey", json!(SLSKD_KEY)),
            ]),
        )
        .await;
        assert_failure(st, &body, "baseUrl");

        let (st, body) = test(
            path,
            imp,
            fields(&[
                ("baseUrl", json!(url)),
                ("apiKey", json!(SLSKD_KEY)),
                ("native", json!(false)),
            ]),
        )
        .await;
        assert_eq!(st, 400, "native:false is not valid: {body}");
    }
}

#[tokio::test]
async fn apple_rows_are_really_tested_on_both_endpoints() {
    let (api, dec) = fake_apple(true).await;
    let good = |extra: &[(&str, Value)]| {
        let mut f = vec![
            ("baseUrl", json!(api)),
            ("developerToken", json!(DEV_TOKEN)),
            ("musicUserToken", json!(USER_TOKEN)),
            ("decryptorUrl", json!(dec)),
        ];
        f.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
        fields(&f)
    };
    for (path, imp) in [
        ("/api/v1/indexer/test", "AppleMusicarrIndexer"),
        ("/api/v1/downloadclient/test", "AppleMusicarrDownloadClient"),
    ] {
        let (st, body) = test(path, imp, good(&[])).await;
        assert_eq!(st, 200, "{imp}: {body}");

        // A rejected developer token (the later field wins in the settings map).
        let (st, body) = test(
            path,
            imp,
            good(&[("developerToken", json!("not-the-token"))]),
        )
        .await;
        assert_failure(st, &body, "developerToken");

        let (st, body) = test(path, imp, good(&[("native", json!(false))])).await;
        assert_eq!(st, 400, "{body}");
    }
    // An unhealthy decryptor fails the client, not the indexer.
    let (api2, dec2) = fake_apple(false).await;
    let f = fields(&[
        ("baseUrl", json!(api2)),
        ("developerToken", json!(DEV_TOKEN)),
        ("musicUserToken", json!(USER_TOKEN)),
        ("decryptorUrl", json!(dec2)),
    ]);
    let (st, body) = test(
        "/api/v1/downloadclient/test",
        "AppleMusicarrDownloadClient",
        f.clone(),
    )
    .await;
    assert_failure(st, &body, "decryptorUrl");
    let (st, _) = test("/api/v1/indexer/test", "AppleMusicarrIndexer", f).await;
    assert_eq!(st, 200);
}
