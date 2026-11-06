//! Minimal structured logging: one JSON object per line on stdout, in the
//! shape Google Cloud Logging understands (`severity` + `message`).
//!
//! Secret values must never be passed to these functions.

use serde_json::{Map, Value, json};

fn emit(severity: &str, message: &str, fields: Value) {
    let mut entry = Map::new();
    entry.insert("severity".into(), json!(severity));
    entry.insert("message".into(), json!(message));
    if let Value::Object(extra) = fields {
        entry.extend(extra);
    }
    println!("{}", Value::Object(entry));
}

pub fn info(message: &str, fields: Value) {
    emit("INFO", message, fields);
}

pub fn warn(message: &str, fields: Value) {
    emit("WARNING", message, fields);
}

pub fn error(message: &str, fields: Value) {
    emit("ERROR", message, fields);
}
