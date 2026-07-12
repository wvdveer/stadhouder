use std::panic;

use serde::Serialize;
use serde_json::json;

/// A successful JSON handler result: the data to return, plus an optional
/// `Set-Cookie` header value for the endpoints that establish a session.
pub struct JsonSuccess<T> {
    data: T,
    set_cookie: Option<String>,
}

impl<T> JsonSuccess<T> {
    pub fn new(data: T) -> Self {
        JsonSuccess {
            data,
            set_cookie: None,
        }
    }

    pub fn with_cookie(data: T, set_cookie: String) -> Self {
        JsonSuccess {
            data,
            set_cookie: Some(set_cookie),
        }
    }
}

/// Runs `f` and always returns HTTP 200 with a JSON envelope
/// `{ok, error, data}`, catching panics along the way. Real failures show up
/// in the response body instead of an opaque 500, since cPanel's error logs
/// don't reliably capture CGI panics.
pub fn json_handler<F, T>(f: F) -> cgi::Response
where
    F: FnOnce() -> Result<JsonSuccess<T>, String>,
    T: Serialize,
{
    let outcome = panic::catch_unwind(panic::AssertUnwindSafe(f));

    let (body, set_cookie) = match outcome {
        Ok(Ok(success)) => (
            json!({"ok": true, "error": null, "data": success.data}),
            success.set_cookie,
        ),
        Ok(Err(e)) => (json!({"ok": false, "error": e, "data": null}), None),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            (
                json!({"ok": false, "error": format!("panic: {msg}"), "data": null}),
                None,
            )
        }
    };

    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());

    let mut builder = cgi::http::Response::builder()
        .status(200)
        .header(cgi::http::header::CONTENT_TYPE, "application/json");

    if let Some(cookie) = set_cookie {
        builder = builder.header(cgi::http::header::SET_COOKIE, cookie);
    }

    builder.body(bytes).unwrap()
}
