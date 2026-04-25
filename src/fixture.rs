use std::path::Path;

use sonic_rs::JsonValueTrait;

use crate::ws::WebSocketError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsFixture {
    pub source: String,
    pub captured_at_ms: u64,
    pub capture_command: String,
    pub exchange_env: String,
    pub text: String,
}

impl WsFixture {
    pub fn read(path: &Path) -> Result<Self, WebSocketError> {
        let bytes = std::fs::read(path).map_err(|err| {
            WebSocketError::InvalidState(format!(
                "failed to read websocket fixture {}: {err}",
                path.display()
            ))
        })?;
        let root = sonic_rs::from_slice::<sonic_rs::Value>(&bytes).map_err(|err| {
            WebSocketError::ParseFailed(format!(
                "failed to parse websocket fixture {}: {err}",
                path.display()
            ))
        })?;
        let text = root.get("text").ok_or_else(|| {
            WebSocketError::InvalidState(format!(
                "websocket fixture {} missing text",
                path.display()
            ))
        })?;
        Ok(Self {
            source: root
                .get("source")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string(),
            captured_at_ms: root
                .get("captured_at_ms")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            capture_command: root
                .get("capture_command")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            exchange_env: root
                .get("exchange_env")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            text: decode_embedded_json_value(text, path)?,
        })
    }

    pub fn write(&self, path: &Path) -> Result<(), WebSocketError> {
        let text =
            encode_embedded_json_value(self.text.as_str()).map_err(WebSocketError::InvalidState)?;
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "capture_command": self.capture_command,
            "captured_at_ms": self.captured_at_ms,
            "exchange_env": self.exchange_env,
            "source": self.source,
            "text": text,
        }))
        .map_err(|err| WebSocketError::ParseFailed(err.to_string()))?;
        std::fs::write(path, bytes).map_err(|err| {
            WebSocketError::InvalidState(format!(
                "failed to write websocket fixture {}: {err}",
                path.display()
            ))
        })
    }
}

fn decode_embedded_json_value(
    value: &sonic_rs::Value,
    path: &Path,
) -> Result<String, WebSocketError> {
    let mut text = if value.is_array() || value.is_object() {
        sonic_rs::to_string(value).map_err(|err| WebSocketError::ParseFailed(err.to_string()))?
    } else {
        value
            .as_str()
            .ok_or_else(|| {
                WebSocketError::InvalidState(format!(
                    "websocket fixture {} text must be a JSON string, object, or array",
                    path.display()
                ))
            })?
            .to_string()
    };
    for _ in 0..4 {
        if let Ok(decoded) = sonic_rs::from_str::<sonic_rs::Value>(text.as_str()) {
            if decoded.is_array() || decoded.is_object() {
                break;
            }
            if let Some(inner) = decoded.as_str() {
                if inner == text {
                    break;
                }
                text = inner.to_string();
                continue;
            }
        }
        let wrapped = format!("\"{text}\"");
        let Ok(decoded) = sonic_rs::from_str::<String>(wrapped.as_str()) else {
            break;
        };
        if decoded == text {
            break;
        }
        text = decoded;
    }
    Ok(text)
}

fn encode_embedded_json_value(raw: &str) -> Result<serde_json::Value, String> {
    let normalized = decode_embedded_json_string(raw);
    let json = serde_json::from_str::<serde_json::Value>(normalized.as_str())
        .map_err(|err| err.to_string())?;
    match json {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => Ok(json),
        _ => Ok(serde_json::Value::String(normalized)),
    }
}

fn decode_embedded_json_string(raw: &str) -> String {
    let mut current = raw.to_string();
    for _ in 0..4 {
        match serde_json::from_str::<serde_json::Value>(current.as_str()) {
            Ok(serde_json::Value::Object(_)) | Ok(serde_json::Value::Array(_)) => return current,
            Ok(serde_json::Value::String(inner)) => {
                if inner == current {
                    return current;
                }
                current = inner;
            }
            Ok(_) | Err(_) => return current,
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::WsFixture;

    fn temp_fixture_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("shared-ws-{name}-{}.json", std::process::id()));
        path
    }

    #[test]
    fn ws_fixture_round_trips_nested_object_text() {
        let path = temp_fixture_path("object");
        let fixture = WsFixture {
            source: "live_capture".to_string(),
            captured_at_ms: 1,
            capture_command: "capture".to_string(),
            exchange_env: "deribit_testnet".to_string(),
            text: "{\"jsonrpc\":\"2.0\",\"method\":\"public/test\"}".to_string(),
        };
        fixture.write(path.as_path()).expect("fixture should write");
        let written = std::fs::read_to_string(path.as_path()).expect("fixture should read");
        assert!(written.contains(
            "\n  \"text\": {\n    \"jsonrpc\": \"2.0\",\n    \"method\": \"public/test\"\n  }"
        ));
        let decoded = WsFixture::read(path.as_path()).expect("fixture should decode");
        assert_eq!(decoded.text, fixture.text);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ws_fixture_reads_double_encoded_text() {
        let path = temp_fixture_path("double");
        std::fs::write(
            path.as_path(),
            r#"{"source":"live_capture","captured_at_ms":1,"capture_command":"capture","exchange_env":"deribit_testnet","text":"\"{\\\"jsonrpc\\\":\\\"2.0\\\"}\""}"#,
        )
        .expect("fixture should write");
        let decoded = WsFixture::read(path.as_path()).expect("fixture should decode");
        assert_eq!(decoded.text, r#"{"jsonrpc":"2.0"}"#);
        let _ = std::fs::remove_file(path);
    }
}
