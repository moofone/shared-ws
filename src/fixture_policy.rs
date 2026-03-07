use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use sonic_rs::JsonValueTrait;

use crate::core::WebSocketError;

const SHARED_WS_FIXTURE_CAPTURE_MODE_ENV: &str = "SHARED_WS_FIXTURE_CAPTURE_MODE";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsFixtureRequirement {
    pub contract_id: String,
    pub success_path: PathBuf,
    pub error_path: Option<PathBuf>,
}

#[derive(Default)]
struct WsFixtureRegistry {
    requirements: Vec<WsFixtureRequirement>,
}

fn registry() -> &'static Mutex<WsFixtureRegistry> {
    static REGISTRY: OnceLock<Mutex<WsFixtureRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(WsFixtureRegistry::default()))
}

pub fn register_required_ws_contracts(requirements: impl IntoIterator<Item = WsFixtureRequirement>) {
    let mut guard = registry().lock().expect("ws fixture registry poisoned");
    guard.requirements = requirements.into_iter().collect();
}

pub fn required_ws_contracts() -> Vec<WsFixtureRequirement> {
    registry()
        .lock()
        .expect("ws fixture registry poisoned")
        .requirements
        .clone()
}

pub fn clear_required_ws_contracts_for_tests() {
    registry()
        .lock()
        .expect("ws fixture registry poisoned")
        .requirements
        .clear();
}

pub fn fixture_capture_mode_enabled() -> bool {
    std::env::var(SHARED_WS_FIXTURE_CAPTURE_MODE_ENV)
        .ok()
        .map(|raw| matches!(raw.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn ensure_live_capture_fixture(path: &PathBuf) -> Result<(), WebSocketError> {
    let bytes = std::fs::read(path).map_err(|err| {
        WebSocketError::InvalidState(format!(
            "live websocket blocked: failed to read fixture {}: {err}",
            path.display()
        ))
    })?;
    let root = sonic_rs::from_slice::<sonic_rs::Value>(&bytes).map_err(|err| {
        WebSocketError::InvalidState(format!(
            "live websocket blocked: failed to parse fixture {}: {err}",
            path.display()
        ))
    })?;
    let source = root.get("source").and_then(|value| value.as_str()).unwrap_or("");
    let captured_at_ms = root
        .get("captured_at_ms")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let capture_command = root
        .get("capture_command")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let exchange_env = root
        .get("exchange_env")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if source != "live_capture"
        || captured_at_ms == 0
        || capture_command.trim().is_empty()
        || exchange_env.trim().is_empty()
    {
        return Err(WebSocketError::InvalidState(format!(
            "live websocket blocked: fixture {} is not compliant live-capture provenance",
            path.display()
        )));
    }
    Ok(())
}

pub fn ensure_live_ws_allowed() -> Result<(), WebSocketError> {
    if fixture_capture_mode_enabled() {
        return Ok(());
    }
    let guard = registry().lock().expect("ws fixture registry poisoned");
    if guard.requirements.is_empty() {
        return Err(WebSocketError::InvalidState(
            "live websocket blocked: no required fixture contracts registered".to_string(),
        ));
    }
    for item in &guard.requirements {
        let success_exists = item.success_path.exists();
        let error_exists = item.error_path.as_ref().map(|p| p.exists()).unwrap_or(true);
        if !(success_exists && error_exists) {
            return Err(WebSocketError::InvalidState(format!(
                "live websocket blocked: missing fixture files for contract={} success={} error={}",
                item.contract_id,
                item.success_path.display(),
                item.error_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<not-required>".to_string())
            )));
        }
        ensure_live_capture_fixture(&item.success_path)?;
        if let Some(error_path) = &item.error_path {
            ensure_live_capture_fixture(error_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shared-ws-fixture-policy-{}-{}",
            std::process::id(),
            name
        ))
    }

    fn write_fixture(path: &Path, source: &str) {
        let body = sonic_rs::to_vec(&sonic_rs::json!({
            "source": source,
            "captured_at_ms": if source == "live_capture" { 1_u64 } else { 0_u64 },
            "capture_command": if source == "live_capture" { "capture" } else { "" },
            "exchange_env": if source == "live_capture" { "deribit_testnet" } else { "" },
            "text": "{}"
        }))
        .expect("serialize fixture");
        std::fs::write(path, body).expect("write fixture");
    }

    #[test]
    fn live_ws_requires_registered_contracts() {
        clear_required_ws_contracts_for_tests();
        let err = ensure_live_ws_allowed().expect_err("missing registry should fail");
        assert!(err.to_string().contains("no required fixture contracts registered"));
    }

    #[test]
    fn live_ws_rejects_non_live_capture_fixtures() {
        clear_required_ws_contracts_for_tests();
        let success = temp_path("success.json");
        let error = temp_path("error.json");
        let _ = std::fs::remove_file(&success);
        let _ = std::fs::remove_file(&error);
        write_fixture(success.as_path(), "synthesized");
        write_fixture(error.as_path(), "live_capture");
        register_required_ws_contracts([WsFixtureRequirement {
            contract_id: "contract-a".to_string(),
            success_path: success.clone(),
            error_path: Some(error.clone()),
        }]);
        let err = ensure_live_ws_allowed().expect_err("non-live provenance should fail");
        assert!(err
            .to_string()
            .contains("not compliant live-capture provenance"));
        let _ = std::fs::remove_file(success);
        let _ = std::fs::remove_file(error);
    }

    #[test]
    fn live_ws_accepts_live_capture_fixtures() {
        clear_required_ws_contracts_for_tests();
        let success = temp_path("good-success.json");
        let error = temp_path("good-error.json");
        let _ = std::fs::remove_file(&success);
        let _ = std::fs::remove_file(&error);
        write_fixture(success.as_path(), "live_capture");
        write_fixture(error.as_path(), "live_capture");
        register_required_ws_contracts([WsFixtureRequirement {
            contract_id: "contract-a".to_string(),
            success_path: success.clone(),
            error_path: Some(error.clone()),
        }]);
        ensure_live_ws_allowed().expect("live-captured fixtures should pass");
        let _ = std::fs::remove_file(success);
        let _ = std::fs::remove_file(error);
    }
}
