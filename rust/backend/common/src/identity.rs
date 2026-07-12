//! Verifies who a caller actually is by asking a trusted, same-host
//! identity endpoint (`Config::user_id_url`) - rather than trusting a
//! self-asserted user_id - forwarding the caller's own Cookie header so
//! the answer reflects their real session. Used by the client CGI
//! program's `connect` action.
//!
//! Stadhouder assumes nothing about the endpoint's response beyond it
//! being JSON: `Config::user_id_json` names a dot-separated field path
//! (e.g. `data.id`) to read the verified id from. A missing, null, or
//! empty-string value at that path means "not signed in" - not an error,
//! just no identity to report. Only a transport failure, a non-JSON body,
//! or a non-string value at the path is an `Err`.
use serde_json::Value;

/// Calls `url`, forwarding `cookie_header` exactly as the original request
/// had it, and reads `field_path` (dot-separated, e.g. `data.id`) out of
/// the JSON response. `Some(id)` when present and non-empty, `None` when
/// missing/null/empty (no session), `Err` on anything that stops the
/// check from completing at all.
pub fn verify_user(
    url: &str,
    cookie_header: Option<&str>,
    field_path: &str,
) -> Result<Option<String>, String> {
    let mut request = ureq::get(url);
    if let Some(cookie) = cookie_header {
        request = request.header("Cookie", cookie);
    }
    let mut response = request
        .call()
        .map_err(|e| format!("identity check request to {url} failed: {e}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("identity check response from {url} unreadable: {e}"))?;

    parse_field(&body, field_path).map_err(|e| format!("identity check at {url}: {e}"))
}

fn parse_field(body: &str, field_path: &str) -> Result<Option<String>, String> {
    let parsed: Value =
        serde_json::from_str(body).map_err(|_| "response is not JSON".to_string())?;

    match resolve_path(&parsed, field_path) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("field '{field_path}' is not a string: {other}")),
    }
}

/// Walks `path`'s dot-separated segments as successive object-key lookups
/// (e.g. `"data.id"` -> `value["data"]["id"]`). `None` if any segment is
/// missing or the value at that point isn't an object.
fn resolve_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_nested_field_by_dotted_path() {
        // Exactly KeyScarf's api_session_check shape - the concrete case
        // USER_ID_URL/USER_ID_JSON are configured for, but nothing here
        // depends on the rest of that shape.
        let body = r#"{"ok":true,"logged_in":true,"data":{"id":"00000000-0000-0000-0000-000000000002","email":"alice@example.com"}}"#;
        assert_eq!(
            parse_field(body, "data.id").unwrap(),
            Some("00000000-0000-0000-0000-000000000002".to_string())
        );
    }

    #[test]
    fn reads_a_top_level_field() {
        assert_eq!(
            parse_field(r#"{"user_id":"abc"}"#, "user_id").unwrap(),
            Some("abc".to_string())
        );
    }

    #[test]
    fn null_field_means_no_session() {
        let body = r#"{"ok":true,"logged_in":false,"data":null}"#;
        assert_eq!(parse_field(body, "data.id").unwrap(), None);
    }

    #[test]
    fn missing_field_means_no_session() {
        assert_eq!(parse_field(r#"{"ok":true}"#, "data.id").unwrap(), None);
    }

    #[test]
    fn empty_string_means_no_session() {
        assert_eq!(parse_field(r#"{"id":""}"#, "id").unwrap(), None);
    }

    #[test]
    fn non_string_field_is_an_error() {
        assert!(parse_field(r#"{"data":{"id":12345}}"#, "data.id").is_err());
    }

    #[test]
    fn a_path_through_a_non_object_is_missing_not_an_error() {
        // "data" is a string here, not an object - "data.id" simply
        // doesn't resolve, same as any other missing path.
        assert_eq!(parse_field(r#"{"data":"oops"}"#, "data.id").unwrap(), None);
    }

    #[test]
    fn non_json_body_is_an_error() {
        assert!(parse_field("<html>not json</html>", "data.id").is_err());
    }
}
