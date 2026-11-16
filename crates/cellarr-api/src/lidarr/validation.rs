//! Lidarr's validation failures: a 400 whose body is an array of per-property
//! findings, which clients (and Lidarr's own UI) show beside the offending field.
//! The shape and the validator codes are interface facts read off a running Lidarr.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// A collection of validation findings; empty means the request is fine.
#[derive(Default)]
pub struct Failures(Vec<Value>);

fn spaced(property: &str) -> String {
    let mut out = String::new();
    for (i, c) in property.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

impl Failures {
    /// No findings yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a finding.
    pub fn add(&mut self, property: &str, message: &str, code: &str, attempted: Option<Value>) {
        let label = spaced(property);
        let value = attempted.clone().unwrap_or(Value::Null);
        let mut v = json!({
            "propertyName": property,
            "errorMessage": message,
            "severity": "error",
            "errorCode": code,
            "formattedMessageArguments": [],
            "formattedMessagePlaceholderValues": { "propertyName": label, "propertyValue": value },
        });
        if let Some(a) = attempted {
            v["attemptedValue"] = a;
        }
        self.0.push(v);
    }

    /// `property` must be greater than zero.
    pub fn positive(&mut self, property: &str, value: i64) {
        if value <= 0 {
            self.add(
                property,
                &format!("'{}' must be greater than '0'.", spaced(property)),
                "GreaterThanValidator",
                Some(json!(value)),
            );
        }
    }

    /// `property` must not be empty.
    pub fn not_empty(&mut self, property: &str, value: &str) {
        if value.trim().is_empty() {
            self.add(
                property,
                &format!("'{}' must not be empty.", spaced(property)),
                "NotEmptyValidator",
                Some(json!(value)),
            );
        }
    }

    /// Nothing was found wrong.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `Ok` when empty, else the 400 response.
    pub fn check(self) -> Result<(), Failures> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl IntoResponse for Failures {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(Value::Array(self.0))).into_response()
    }
}
