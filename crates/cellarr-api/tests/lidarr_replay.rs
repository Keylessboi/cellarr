//! cellarr's Lidarr face against what a real Lidarr answered, offline.
//!
//! `scripts/lidarr-compare --record` stores the status and the JSON *shape* (key names
//! and value types) of a real Lidarr's answer to every request in
//! `contracts/lidarr-scenario.json`; they are committed under `contracts/real-lidarr/`.
//! This test runs the same requests against an in-process cellarr and compares.
//!
//! Differences cellarr has not closed are listed in `contracts/real-lidarr-baseline.json`.
//! The test is a ratchet: a new difference fails it, and so does a listed one that has
//! been fixed (rerun with `UPDATE_BASELINE=1` to record the improvement). Refresh the
//! recordings from a pinned Lidarr when a new release ships and the diff shows exactly
//! what moved.

mod lidarr_common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use cellarr_api::AuthConfig;
use serde_json::{json, Map, Value};

fn contracts() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/contracts")
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(n) => {
            if n.as_f64().is_some_and(|f| f.fract() == 0.0) {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

/// Key -> type, recursively; an array merges its elements' shapes.
fn shape(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), shape(x))).collect()),
        Value::Array(a) => {
            let mut merged = json!({});
            for e in a {
                let s = shape(e);
                if s.is_object() && merged.is_object() {
                    merged = merge(&merged, &s);
                } else if is_empty(&merged) {
                    merged = s;
                }
            }
            json!(["[]", merged])
        }
        other => json!(type_of(other)),
    }
}

fn merge(a: &Value, b: &Value) -> Value {
    let (Some(ao), Some(bo)) = (a.as_object(), b.as_object()) else {
        return a.clone();
    };
    let mut out: Map<String, Value> = ao.clone();
    for (k, v) in bo {
        let merged = match out.get(k) {
            Some(cur) if cur.is_object() && v.is_object() => merge(cur, v),
            Some(cur) => cur.clone(),
            None => v.clone(),
        };
        out.insert(k.clone(), merged);
        if ao.get(k) == Some(&json!("null")) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn brief(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => "object".into(),
        _ => "array".into(),
    }
}

/// Differences of shape `a` (real Lidarr) against `b` (cellarr).
fn diff(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(ao), Value::Object(bo)) => {
            let mut keys: Vec<&String> = ao.keys().chain(bo.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                match (ao.get(k), bo.get(k)) {
                    (Some(x), None) => out.push(format!(
                        "{path}.{k}: missing from cellarr (Lidarr: {})",
                        brief(x)
                    )),
                    (None, Some(_)) => out.push(format!("{path}.{k}: extra in cellarr")),
                    (Some(x), Some(y)) => diff(x, y, &format!("{path}.{k}"), out),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(aa), Value::Array(ba)) if aa.len() == 2 && ba.len() == 2 => {
            let (ae, be) = (!is_empty(&aa[1]), !is_empty(&ba[1]));
            if ae != be {
                out.push(format!(
                    "{path}[]: Lidarr {} elements, cellarr {}",
                    if ae { "has" } else { "has no" },
                    if be { "has" } else { "has none" }
                ));
            } else if ae {
                diff(&aa[1], &ba[1], &format!("{path}[]"), out);
            }
        }
        _ => {
            if a != b && a != &json!("null") && b != &json!("null") {
                out.push(format!("{path}: Lidarr {}, cellarr {}", brief(a), brief(b)));
            }
        }
    }
}

fn substitute(s: &str, ctx: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in ctx {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[tokio::test]
async fn cellarr_answers_like_the_recorded_real_lidarr() {
    let l = lidarr_common::start(AuthConfig::disabled()).await;
    let root = tempfile::tempdir().unwrap();
    let mut ctx = BTreeMap::new();
    ctx.insert(
        "root".to_string(),
        root.path().to_string_lossy().to_string(),
    );

    let steps: Vec<Value> =
        serde_json::from_slice(&std::fs::read(contracts().join("lidarr-scenario.json")).unwrap())
            .unwrap();
    let baseline_path = contracts().join("real-lidarr-baseline.json");
    let baseline: BTreeMap<String, Vec<String>> = std::fs::read(&baseline_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    let mut observed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for step in &steps {
        let name = step["name"].as_str().unwrap().to_string();
        let method = step["method"].as_str().unwrap_or("GET");
        let path = substitute(step["path"].as_str().unwrap(), &ctx);
        let recorded: Value = serde_json::from_slice(
            &std::fs::read(contracts().join("real-lidarr").join(format!("{name}.json")))
                .unwrap_or_else(|_| panic!("no recording for step {name}")),
        )
        .unwrap();

        let mut req = l
            .client
            .request(method.parse().unwrap(), format!("{}/api/v1{path}", l.base));
        let mut send_key = true;
        if let Some(h) = step["headers"].as_object() {
            if h.get("X-Api-Key").is_some_and(Value::is_null) {
                send_key = false;
            }
        }
        if send_key {
            req = req.header("X-Api-Key", lidarr_common::KEY);
        }
        req = req.header("Content-Type", "application/json");
        if let Some(raw) = step["raw"].as_str() {
            req = req.body(raw.to_string());
        } else if step.get("body").is_some() {
            let text = substitute(&step["body"].to_string(), &ctx);
            req = req.body(text);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let body: Option<Value> = serde_json::from_str(&text).ok();

        if let (Some(save), Some(from), Some(b)) =
            (step["save"].as_str(), step["from"].as_str(), body.as_ref())
        {
            // `j['key']`
            let key = from.trim_start_matches("j['").trim_end_matches("']");
            let v = b
                .get(key)
                .map(|v| v.to_string().trim_matches('"').to_string());
            ctx.insert(save.to_string(), v.unwrap_or_else(|| "0".into()));
        }

        let mut diffs = Vec::new();
        let rec_status = recorded["status"].as_u64().unwrap_or(0) as u16;
        if rec_status != status {
            diffs.push(format!("status: Lidarr {rec_status}, cellarr {status}"));
        } else if let (Some(rs), Some(b)) = (recorded.get("shape"), body.as_ref()) {
            if !rs.is_null() && !step["shape_only_status"].as_bool().unwrap_or(false) {
                diff(rs, &shape(b), "", &mut diffs);
            }
        }
        if !diffs.is_empty() {
            observed.insert(name, diffs);
        }
    }

    if std::env::var("UPDATE_BASELINE").is_ok() {
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&observed).unwrap() + "\n",
        )
        .unwrap();
        return;
    }

    let mut new = Vec::new();
    let mut fixed = Vec::new();
    for (name, diffs) in &observed {
        let known = baseline.get(name).cloned().unwrap_or_default();
        for d in diffs {
            if !known.contains(d) {
                new.push(format!("{name}: {d}"));
            }
        }
    }
    for (name, known) in &baseline {
        let now = observed.get(name).cloned().unwrap_or_default();
        for d in known {
            if !now.contains(d) {
                fixed.push(format!("{name}: {d}"));
            }
        }
    }
    let identical = steps.len() - observed.len();
    eprintln!("{identical}/{} requests match the real Lidarr", steps.len());
    assert!(
        new.is_empty(),
        "cellarr drifted from the recorded real Lidarr in {} place(s):\n  {}\n(fix it, or if it is intended rerun with UPDATE_BASELINE=1)",
        new.len(),
        new.join("\n  ")
    );
    assert!(
        fixed.is_empty(),
        "{} known difference(s) are fixed; rerun with UPDATE_BASELINE=1 to record that:\n  {}",
        fixed.len(),
        fixed.join("\n  ")
    );
}
