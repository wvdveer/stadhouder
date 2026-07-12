//! The wire-level heartbeat message. The client CGI program's `poll`
//! action sends one whenever its blocking wait for messages times out
//! empty - it keeps the connection from tripping the service's inactivity
//! disconnect without ever reaching the state engine: the service filters
//! it out (via [`is_heartbeat`]) before building the batch for
//! `StadhouderStateEngine::process_messages`, though it still counts as
//! communication for the purpose of the connection's last-message time.
use serde_json::{json, Value};

const HEARTBEAT_KEY: &str = "stadhouder_heartbeat";

/// The exact JSON frame the client sends.
pub fn message() -> Value {
    json!({ HEARTBEAT_KEY: true })
}

/// Whether an inbound message is a heartbeat.
pub fn is_heartbeat(message: &Value) -> bool {
    message.get(HEARTBEAT_KEY).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_recognized_as_heartbeat() {
        assert!(is_heartbeat(&message()));
    }

    #[test]
    fn ordinary_message_is_not_a_heartbeat() {
        assert!(!is_heartbeat(&json!({"cmd": "get"})));
    }

    #[test]
    fn message_missing_the_key_is_not_a_heartbeat() {
        assert!(!is_heartbeat(&json!({})));
    }
}
