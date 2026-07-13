//! The one client CGI program - the HTTP side of stadhouder. POST one JSON
//! request as the raw body, naming the action via "kind":
//!   {"kind":"status"}                                            -> {"service":..,"version":..,"test_env":..,"time_ms":..,"service_last_run_ms":..,"service_memory_pct":..}
//!   {"kind":"connect","user_id":"..."}                           -> {"connection_id":"<uuid>"}
//!   {"kind":"send","connection_id":"...","message":{...}}        -> {"sent":true}
//!   {"kind":"poll","connection_id":"...","wait_ms":<engine ms>}  -> {"messages":[...]}
//!   {"kind":"close","connection_id":"..."}                       -> {"closed":true}
//!
//! status is a health/readiness check: the engine name, version, whether
//! this is a test instance, the engine's current time (simulated on a
//! test instance), when the cron-driven service last ran, and its
//! last-measured memory usage as a percentage of MEMORY_LIMIT_MB (null
//! when that's unset, or when no instance has ever run).
//!
//! connect mints the connection uuid and writes the connection's profile
//! file into stadhouder/state/ (uuid, user id, the pipe pair's names, the
//! calendar time established, and - on a test instance - the simtime
//! established); the running service program picks the profile up from
//! there and starts serving the pipes. send is one pipe episode; close
//! deletes the profile, which is how the service learns the client is
//! gone.
//!
//! When `COOKIE_NAME` is set in stadhouder.conf, connect requires a
//! cookie of that name to be present on the request (its value is never
//! inspected, only its presence) - a cheap guard against an unauthenticated
//! caller minting connections directly, on the assumption that anything
//! fronting stadhouder already set that cookie after establishing its own
//! session (e.g. the identity provider's own session cookie).
//!
//! When `USER_ID_URL` (and `USER_ID_JSON`) are also set, connect goes
//! further: it calls that same-host URL, forwarding the request's own
//! Cookie header, and reads the verified identity out of the JSON
//! response at the `USER_ID_JSON` field path (e.g. `data.id`) - see
//! `common::identity`. That verified id REPLACES whatever user_id the
//! request itself supplied; a response with no identity at that path
//! means "not signed in", and the connect attempt is rejected.
//!
//! When `MEMORY_LIMIT_MB` is set, connect also checks the service's
//! last-measured memory usage (`memory_pct` in the service flag file,
//! written by the running service - see `common::state::ServiceFlag`) and
//! rejects the attempt once it reaches `MEMORY_REJECT_PCT` of the limit
//! (default 100%). No flag (the service isn't currently running) never
//! blocks a connect attempt.
//!
//! poll blocks for up to wait_ms (engine time, scaled by TIME_FACTOR),
//! returning the moment a message is available - or, if the wait elapses
//! with nothing, sends a "heartbeat" message to the service first and
//! then returns an empty array. The heartbeat refreshes the connection's
//! last-communication time (keeping it under the service's inactivity
//! disconnect) without ever reaching the state engine.
extern crate cgi;

use std::time::{Duration, Instant};

use common::handler::{json_handler, JsonSuccess};
use common::state::ConnectionProfile;
use common::{cookie, heartbeat, identity, layout, pipes, state, time, Config};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ClientRequest {
    Status,
    Connect { user_id: String },
    Send { connection_id: String, message: Value },
    Poll { connection_id: String, wait_ms: i64 },
    Close { connection_id: String },
}

/// The connection's profile, or the uniform "unknown connection" error the
/// caller sees whether the id never existed or the connection was closed.
fn profile_for(connection_id: &str) -> Result<ConnectionProfile, String> {
    let path = ConnectionProfile::path_in(&state::state_dir()?, connection_id);
    if !path.exists() {
        return Err("unknown connection".to_string());
    }
    ConnectionProfile::load(&path)
}

cgi::cgi_main! { |request: cgi::Request| -> cgi::Response {
    json_handler(|| {
        let body = std::str::from_utf8(request.body())
            .map_err(|_| "request must be UTF-8".to_string())?;
        let client_request: ClientRequest =
            serde_json::from_str(body).map_err(|e| format!("bad request: {e}"))?;

        let reply = match client_request {
            ClientRequest::Status => {
                let config = Config::load()?;
                let memory_pct = if config.memory_limit_bytes.is_some() {
                    state::read_service_flag(&state::state_dir()?)?.map(|flag| flag.memory_pct)
                } else {
                    None
                };
                json!({
                    "service": "stadhouder",
                    "version": env!("CARGO_PKG_VERSION"),
                    "test_env": config.test_env,
                    "time_ms": time::now_millis(&config)?,
                    "service_last_run_ms": state::last_run_ms()?,
                    "service_memory_pct": memory_pct,
                })
            }
            ClientRequest::Connect { user_id } => {
                let config = Config::load()?;

                if let Some(flag) = state::read_service_flag(&state::state_dir()?)? {
                    if flag.memory_pct >= config.memory_reject_pct {
                        return Err("service memory limit exceeded".to_string());
                    }
                }

                let cookie_header = request
                    .headers()
                    .get(cgi::http::header::COOKIE)
                    .and_then(|v| v.to_str().ok());

                if let Some(cookie_name) = &config.cookie_name {
                    if !cookie::has_cookie(cookie_header, cookie_name) {
                        return Err("missing required cookie".to_string());
                    }
                }

                // A verified identity, when configured, replaces whatever
                // user_id the request itself claimed.
                let user_id = match &config.user_id_url {
                    None => user_id,
                    Some(user_id_url) => {
                        let field_path = config.user_id_json.as_deref().ok_or_else(|| {
                            "USER_ID_URL is configured but USER_ID_JSON is not".to_string()
                        })?;
                        let url = common::request::same_host_url(user_id_url)?;
                        match identity::verify_user(&url, cookie_header, field_path)? {
                            Some(verified) => verified,
                            None => return Err("not signed in".to_string()),
                        }
                    }
                };

                let connection_id = uuid::Uuid::new_v4().to_string();
                let names = pipes::pipe_names_for(&layout::stadhouder_dir()?, &connection_id);

                let established_sim_ms = if config.test_env {
                    Some(time::now_millis(&config)?)
                } else {
                    None
                };
                ConnectionProfile {
                    connection_id: connection_id.clone(),
                    user_id,
                    client_to_server_pipe: names.client_to_server,
                    server_to_client_pipe: names.server_to_client,
                    established_ms: time::calendar_now_millis(),
                    established_sim_ms,
                }
                .save(&state::state_dir()?)?;

                json!({"connection_id": connection_id})
            }
            ClientRequest::Send { connection_id, message } => {
                let profile = profile_for(&connection_id)?;
                let factor = time::wait_factor(&Config::load()?)?;
                pipes::client_send(&profile.client_to_server_pipe, &message.to_string(), factor)?;
                json!({"sent": true})
            }
            ClientRequest::Poll { connection_id, wait_ms } => {
                let profile = profile_for(&connection_id)?;
                let factor = time::wait_factor(&Config::load()?)?;
                let deadline = Instant::now()
                    + Duration::from_millis(time::scale_wait_ms(wait_ms.max(0) as u64, factor));

                let mut messages: Value = json!([]);
                loop {
                    let frame = pipes::client_receive(&profile.server_to_client_pipe, factor)?;
                    let parsed: Value = serde_json::from_str(&frame)
                        .map_err(|_| "service reply is not JSON".to_string())?;
                    let has_content = parsed.as_array().map(|a| !a.is_empty()).unwrap_or(false);
                    if has_content {
                        messages = parsed;
                        break;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }

                let empty = messages.as_array().map(|a| a.is_empty()).unwrap_or(true);
                if empty {
                    pipes::client_send(
                        &profile.client_to_server_pipe,
                        &heartbeat::message().to_string(),
                        factor,
                    )?;
                }

                json!({"messages": messages})
            }
            ClientRequest::Close { connection_id } => {
                if !state::delete_connection_profile(&state::state_dir()?, &connection_id)? {
                    return Err("unknown connection".to_string());
                }
                json!({"closed": true})
            }
        };

        Ok(JsonSuccess::new(reply))
    })
} }
