use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_ws_requires_registered_contracts() {
        clear_required_ws_contracts_for_tests();
        let err = ensure_live_ws_allowed().expect_err("missing registry should fail");
        assert!(err.to_string().contains("no required fixture contracts registered"));
    }
}
