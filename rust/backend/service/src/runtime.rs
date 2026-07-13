//! stadhouder_run's internals: a SINGLE-THREADED loop-and-sleep runtime.
//! Cron starts the service executable every minute; each instance:
//!
//! - checks the service flag file in stadhouder/state/ - a fresh flag
//!   (updated less than two minutes ago) means the previous instance is
//!   still operating, and this one terminates at once. Otherwise it writes
//!   its own flag, and keeps updating it every twenty seconds while it
//!   runs.
//! - scans for client connections (the connection profile files in
//!   stadhouder/state/, written by the client CGI program) every four
//!   seconds. If none exist 56 seconds after start, it removes its flag
//!   and terminates - the next cron start takes over. While connections
//!   exist it services them and does not terminate until all are closed.
//! - services every connection's pipe pair each tick, non-blockingly:
//!   arrived frames become engine messages; a waiting poll client gets the
//!   connection's queued messages (one frame holding a JSON array).
//! - performs service-initiated disconnections - removing the connection's
//!   profile file and pipes - when the engine orders a close, and also
//!   when a client has not sent a message for two minutes.
//! - a profile that disappears is a client-side close (deleting it is
//!   exactly what the CGI close request does).
//!
//! Every closed connection is reported to the engine's next
//! `process_messages` call, tagged with why (see [`crate::CloseReason`]).
//! Client-side and idle-timeout closes are discovered before that call and
//! go straight into its `closed_connections`; an engine-requested close is
//! actioned (pipes and profile torn down) the moment it's returned, but
//! can only be reported to the FOLLOWING call - the one that requested it
//! is already past building its own `closed_connections` by the time its
//! output comes back. `pending_engine_closes` carries those over.
//!
//! Heartbeat frames from the client (`common::heartbeat`) update a
//! connection's last-message time like any other traffic but are filtered
//! out before reaching the engine - see the inbound-draining loop below.
//!
//! All the durations above are engine time: on a test instance they scale
//! with TIME_FACTOR, like every other sleep. Stale profiles left by a
//! previous run are purged at startup - their clients' pipes died with
//! that run, so they must reconnect.
//!
//! The engine's `init` fires once, right after this instance confirms
//! it's the live one (never for one that defers). Its `shutdown` fires
//! once, only at the "no connections left" idle-exit above - not for an
//! engine-ordered shutdown, which the engine already knows about.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::pipes::{self, PipeNames};
use common::state::{self, ConnectionProfile};
use common::{time, Config};
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{
    CloseReason, ClosedConnection, ConnectionId, ExpiredTimer, InboundMessage, NewConnection,
    StadhouderStateEngine,
};

/// The main loop's pace: everything (pipes, timers, the checks below) is
/// looked at this often. Engine time, like all of these.
const TICK_MS: i64 = 250;
/// How often the state directory is scanned for connection profiles.
const SCAN_INTERVAL_MS: i64 = 4_000;
/// How often the running instance refreshes its service flag file.
const FLAG_UPDATE_INTERVAL_MS: i64 = 20_000;
/// A flag younger than this at startup means another instance is live.
const FLAG_STALE_MS: i64 = 2 * 60_000;
/// With no client connections this long after start, the instance exits.
const IDLE_EXIT_MS: i64 = 56_000;
/// A client silent for this long is disconnected by the service.
const CLIENT_INACTIVITY_MS: i64 = 2 * 60_000;

struct LiveConnection {
    inbound: pipes::InboundServerPipe,
    outbound: pipes::OutboundServerPipe,
    pipe_names: PipeNames,
    outbox: Vec<Value>,
    /// Engine time of the client's last message (pickup counts as one).
    last_message_ms: i64,
}

struct PendingTimer {
    timer_id: crate::TimerId,
    expiry_ms: i64,
    payload: Value,
}

/// Removes a connection's pipes and profile - the service-initiated
/// disconnection (also the cleanup half of a client-initiated one).
fn teardown(state_dir: &Path, connection_id: &ConnectionId, connection: &LiveConnection) {
    pipes::remove_serving_pipes(&connection.pipe_names);
    if let Err(e) = state::delete_connection_profile(state_dir, connection_id) {
        eprintln!("stadhouder: {e}");
    }
}

/// The connection id a profile filename encodes (dashes restored), if it is
/// one.
fn connection_id_from_filename(path: &Path) -> Option<ConnectionId> {
    let name = path.file_name()?.to_string_lossy();
    let tag = name.strip_prefix("connection_")?.strip_suffix(".json")?;
    Some(tag.replace('_', "-"))
}

/// This process's own RSS as a percentage of `MEMORY_LIMIT_MB` (see
/// `common::config::Config::memory_limit_bytes`). `0.0` when no limit is
/// configured - nobody asked for the measurement, so it isn't taken.
fn memory_pct(sys: &mut System, memory_limit_bytes: Option<u64>) -> f64 {
    let Some(limit) = memory_limit_bytes else {
        return 0.0;
    };
    let Ok(pid) = sysinfo::get_current_pid() else {
        return 0.0;
    };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let Some(process) = sys.process(pid) else {
        return 0.0;
    };
    (process.memory() as f64 / limit as f64) * 100.0
}

pub(crate) fn run_loop(
    engine: &mut dyn StadhouderStateEngine,
    config: &Config,
    stadhouder_dir: &Path,
) -> Result<(), String> {
    let state_dir = stadhouder_dir.join("state");
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| format!("failed to create {}: {e}", state_dir.display()))?;

    let mut started_ms = time::now_millis(config)?;
    let mut sys = System::new();

    // Singleton check: a fresh flag means the previous cron start is still
    // operating - this instance's job is to not exist. A flag from the
    // future (simulated time jumped backwards since it was written) can't
    // prove liveness, so it counts as stale.
    if let Some(flag) = state::read_service_flag(&state_dir)? {
        let age_ms = started_ms - flag.updated_ms;
        if (0..FLAG_STALE_MS).contains(&age_ms) {
            eprintln!("stadhouder: another service instance is operating (flag is fresh); exiting");
            return Ok(());
        }
    }
    let mut current_memory_pct = memory_pct(&mut sys, config.memory_limit_bytes);
    state::write_service_flag(&state_dir, started_ms, current_memory_pct)?;
    let mut flag_updated_ms = started_ms;

    // This instance is now the live one - its one and only init() call.
    engine.init(started_ms);

    // Stale profiles from a previous run: those connections' pipes died
    // with that run, so purge them rather than greet ghosts.
    for stale in state::connection_profile_paths(&state_dir)? {
        eprintln!("stadhouder: purging stale profile {}", stale.display());
        let _ = std::fs::remove_file(stale);
    }

    let mut connections: HashMap<ConnectionId, LiveConnection> = HashMap::new();
    let mut timers: Vec<PendingTimer> = Vec::new();
    // Backdated so the first iteration scans immediately.
    let mut last_scan_ms = started_ms - SCAN_INTERVAL_MS;
    // Engine-requested closes actioned this tick, to be reported as
    // EngineRequested on the NEXT tick's closed_connections (see the
    // module doc comment).
    let mut pending_engine_closes: Vec<ConnectionId> = Vec::new();

    loop {
        let now_ms = time::now_millis(config)?;

        // Simulated time can jump BACKWARDS (a test rewriting the sim-time
        // state files); a schedule baseline left in the future would freeze
        // its interval check until time caught up again. Clamp them all.
        if now_ms < started_ms {
            started_ms = now_ms;
        }
        if now_ms < flag_updated_ms {
            flag_updated_ms = now_ms;
        }
        if now_ms < last_scan_ms {
            last_scan_ms = now_ms - SCAN_INTERVAL_MS;
        }
        for connection in connections.values_mut() {
            if now_ms < connection.last_message_ms {
                connection.last_message_ms = now_ms;
            }
        }

        if now_ms - flag_updated_ms >= FLAG_UPDATE_INTERVAL_MS {
            current_memory_pct = memory_pct(&mut sys, config.memory_limit_bytes);
            state::write_service_flag(&state_dir, now_ms, current_memory_pct)?;
            flag_updated_ms = now_ms;
        }

        // Engine-requested closes deferred from the previous tick (see the
        // module doc comment) - reported first, before this tick's own
        // discoveries.
        let mut closed_connections: Vec<ClosedConnection> = pending_engine_closes
            .drain(..)
            .map(|connection_id| ClosedConnection {
                connection_id,
                reason: CloseReason::EngineRequested,
            })
            .collect();

        // -- Scan for connection profiles (every four seconds) -----------
        let mut new_connections: Vec<NewConnection> = Vec::new();
        if now_ms - last_scan_ms >= SCAN_INTERVAL_MS {
            last_scan_ms = now_ms;

            let mut on_disk: HashMap<ConnectionId, PathBuf> = HashMap::new();
            for path in state::connection_profile_paths(&state_dir)? {
                if let Some(id) = connection_id_from_filename(&path) {
                    on_disk.insert(id, path);
                }
            }

            for (connection_id, path) in &on_disk {
                if connections.contains_key(connection_id) {
                    continue;
                }
                let profile = match ConnectionProfile::load(path) {
                    Ok(profile) => profile,
                    Err(e) => {
                        // Likely mid-write; the next scan will get it whole.
                        eprintln!("stadhouder: skipping profile: {e}");
                        continue;
                    }
                };
                let names = PipeNames {
                    client_to_server: profile.client_to_server_pipe.clone(),
                    server_to_client: profile.server_to_client_pipe.clone(),
                };
                let served = pipes::create_serving_pipes(&names)
                    .and_then(|()| {
                        Ok((
                            pipes::InboundServerPipe::create(&names.client_to_server)?,
                            pipes::OutboundServerPipe::create(&names.server_to_client)?,
                        ))
                    });
                let (inbound, outbound) = match served {
                    Ok(pipes) => pipes,
                    Err(e) => {
                        eprintln!("stadhouder: cannot serve connection {connection_id}: {e}");
                        continue;
                    }
                };
                connections.insert(
                    connection_id.clone(),
                    LiveConnection {
                        inbound,
                        outbound,
                        pipe_names: names,
                        outbox: Vec::new(),
                        last_message_ms: now_ms,
                    },
                );
                new_connections.push(NewConnection {
                    connection_id: connection_id.clone(),
                    user_id: profile.user_id,
                });
            }

            // A vanished profile is a client-side close.
            let gone: Vec<ConnectionId> = connections
                .keys()
                .filter(|id| !on_disk.contains_key(*id))
                .cloned()
                .collect();
            for connection_id in gone {
                if let Some(connection) = connections.remove(&connection_id) {
                    pipes::remove_serving_pipes(&connection.pipe_names);
                }
                closed_connections.push(ClosedConnection {
                    connection_id,
                    reason: CloseReason::ClientDisconnected,
                });
            }
        }

        // -- Drain every connection's inbound pipe (every tick) ----------
        let mut messages: Vec<InboundMessage> = Vec::new();
        for (connection_id, connection) in connections.iter_mut() {
            let frames = match connection.inbound.drain_frames() {
                Ok(frames) => frames,
                Err(e) => {
                    eprintln!("stadhouder: receive on {connection_id} failed: {e}");
                    continue;
                }
            };
            for frame in frames {
                // A heartbeat still counts as communication (it exists
                // purely to refresh this), but is never forwarded to the
                // engine - see common::heartbeat.
                connection.last_message_ms = now_ms;
                match serde_json::from_str::<Value>(&frame) {
                    Ok(message) if common::heartbeat::is_heartbeat(&message) => {}
                    Ok(message) => messages.push(InboundMessage {
                        connection_id: connection_id.clone(),
                        message,
                    }),
                    Err(e) => {
                        eprintln!("stadhouder: non-JSON message from {connection_id} dropped: {e}")
                    }
                }
            }
        }

        // -- Service-initiated disconnection of silent clients ------------
        let inactive: Vec<ConnectionId> = connections
            .iter()
            .filter(|(_, c)| now_ms - c.last_message_ms >= CLIENT_INACTIVITY_MS)
            .map(|(id, _)| id.clone())
            .collect();
        for connection_id in inactive {
            eprintln!("stadhouder: disconnecting {connection_id} (no message for two minutes)");
            if let Some(connection) = connections.remove(&connection_id) {
                teardown(&state_dir, &connection_id, &connection);
            }
            closed_connections.push(ClosedConnection {
                connection_id,
                reason: CloseReason::ServerIdleTimeout,
            });
        }

        // -- Expired timers ------------------------------------------------
        let mut expired: Vec<ExpiredTimer> = Vec::new();
        timers.retain(|t| {
            if t.expiry_ms <= now_ms {
                expired.push(ExpiredTimer {
                    timer_id: t.timer_id.clone(),
                    expiry_ms: t.expiry_ms,
                    payload: t.payload.clone(),
                });
                false
            } else {
                true
            }
        });
        expired.sort_by_key(|t| t.expiry_ms);

        // -- The engine ------------------------------------------------------
        let mut shutdown = false;
        if !new_connections.is_empty()
            || !closed_connections.is_empty()
            || !messages.is_empty()
            || !expired.is_empty()
        {
            let mut output = engine.process_messages(
                now_ms,
                current_memory_pct,
                &new_connections,
                &closed_connections,
                &messages,
                &expired,
            );
            shutdown = output.shutdown;

            // Messages queue before closes, but a service-initiated close
            // tears the pipes down at once - an undelivered outbox goes
            // with it (clients learn of the close by their next request
            // failing).
            for message in output.messages.drain(..) {
                match connections.get_mut(&message.connection_id) {
                    Some(connection) => connection.outbox.push(message.message),
                    None => eprintln!(
                        "stadhouder: engine message for unknown connection {} dropped",
                        message.connection_id
                    ),
                }
            }
            for connection_id in output.close_connections.drain(..) {
                match connections.remove(&connection_id) {
                    Some(connection) => {
                        teardown(&state_dir, &connection_id, &connection);
                        // Can't go in THIS call's closed_connections - it's
                        // already been read - so it's reported next tick.
                        pending_engine_closes.push(connection_id);
                    }
                    None => eprintln!(
                        "stadhouder: engine close for unknown connection {connection_id} ignored"
                    ),
                }
            }
            // Cancellations apply before this batch's new timers are
            // added, so reusing a cancelled id in the same output
            // replaces the old timer rather than immediately erasing the
            // new one.
            if !output.cancel_timers.is_empty() {
                let cancelled: std::collections::HashSet<crate::TimerId> =
                    output.cancel_timers.drain(..).collect();
                timers.retain(|t| !cancelled.contains(&t.timer_id));
            }
            for timer in output.timers.drain(..) {
                timers.push(PendingTimer {
                    timer_id: timer.timer_id,
                    expiry_ms: timer.expiry_ms,
                    payload: timer.payload,
                });
            }
        }

        // -- Answer waiting polls (after the engine, so a poll racing a
        // send already sees what the engine just queued) -------------------
        for (connection_id, connection) in connections.iter_mut() {
            let outbox = &mut connection.outbox;
            let served = connection.outbound.try_serve_poll(|| {
                Value::Array(std::mem::take(outbox)).to_string()
            });
            if let Err(e) = served {
                eprintln!("stadhouder: poll on {connection_id} failed: {e}");
            }
        }

        // -- Ways out ---------------------------------------------------------
        if shutdown {
            for (connection_id, connection) in &connections {
                teardown(&state_dir, connection_id, connection);
            }
            state::remove_service_flag(&state_dir);
            return Ok(());
        }
        if connections.is_empty() && now_ms - started_ms >= IDLE_EXIT_MS {
            eprintln!("stadhouder: no client connections; leaving the field to the next cron start");
            engine.shutdown(now_ms);
            state::remove_service_flag(&state_dir);
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(time::scale_wait_ms(
            TICK_MS as u64,
            time::wait_factor(config)?,
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineOutput, OutboundMessage, TimerId, TimerRequest};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// An engine that greets, echoes, and shuts down on request.
    struct EchoEngine;

    impl StadhouderStateEngine for EchoEngine {
        fn process_messages(
            &mut self,
            _timestamp_ms: i64,
            _memory_pct: f64,
            new_connections: &[NewConnection],
            _closed_connections: &[ClosedConnection],
            messages: &[InboundMessage],
            _expired_timers: &[ExpiredTimer],
        ) -> EngineOutput {
            let mut output = EngineOutput::default();
            for connection in new_connections {
                output.messages.push(OutboundMessage {
                    connection_id: connection.connection_id.clone(),
                    message: json!({"hello": connection.user_id}),
                });
            }
            for message in messages {
                if message.message["cmd"] == "shutdown" {
                    output.shutdown = true;
                } else {
                    output.messages.push(OutboundMessage {
                        connection_id: message.connection_id.clone(),
                        message: json!({"echo": message.message}),
                    });
                }
            }
            output
        }
    }

    fn temp_stadhouder_dir(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("stadhouder-runtime-{tag}-{}", std::process::id()))
            .join("stadhouder")
    }

    /// A fresh service flag means another instance is live: run_loop must
    /// return at once, leaving the flag exactly as it found it.
    #[test]
    fn defers_to_a_live_instance() {
        let stadhouder_dir = temp_stadhouder_dir("flag");
        let state_dir = stadhouder_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let flag_ms = time::calendar_now_millis();
        state::write_service_flag(&state_dir, flag_ms, 0.0).unwrap();

        run_loop(&mut EchoEngine, &Config { test_env: false, ..Default::default() }, &stadhouder_dir).unwrap();

        assert_eq!(
            state::read_service_flag(&state_dir).unwrap().map(|f| f.updated_ms),
            Some(flag_ms),
            "the deferring instance must not touch the live instance's flag"
        );

        let _ = std::fs::remove_dir_all(stadhouder_dir.parent().unwrap());
    }

    /// The full file-driven lifecycle over real pipes: the loop writes its
    /// service flag, picks up a profile written the way the CGI program
    /// writes one, greets, echoes, and cleans everything up at shutdown.
    /// No DOCUMENT_ROOT involved - everything takes explicit paths, and a
    /// non-test-env Config never reads the sim-time state files.
    #[test]
    fn full_connection_lifecycle() {
        let stadhouder_dir = temp_stadhouder_dir("lifecycle");
        let state_dir = stadhouder_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let loop_dir = stadhouder_dir.clone();
        let loop_thread = std::thread::spawn(move || {
            run_loop(&mut EchoEngine, &Config { test_env: false, ..Default::default() }, &loop_dir)
        });

        // Wait for startup (the flag appearing) before writing the
        // profile - the loop purges pre-existing profiles as stale.
        for _ in 0..100 {
            if state::read_service_flag(&state_dir).unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(state::read_service_flag(&state_dir).unwrap().is_some());

        let connection_id = "00000000-0000-4000-8000-000000000042".to_string();
        let names = pipes::pipe_names_for(&stadhouder_dir, &connection_id);
        ConnectionProfile {
            connection_id: connection_id.clone(),
            user_id: "alice".to_string(),
            client_to_server_pipe: names.client_to_server.clone(),
            server_to_client_pipe: names.server_to_client.clone(),
            established_ms: time::calendar_now_millis(),
            established_sim_ms: None,
        }
        .save(&state_dir)
        .unwrap();

        // The loop discovers the profile within its scan interval; the
        // clients' own retry windows cover the wait.
        let greeting = pipes::client_receive(&names.server_to_client, 1.0).unwrap();
        assert!(greeting.contains(r#""hello":"alice""#), "got: {greeting}");

        // The instance is flying its flag while it runs.
        assert!(state::read_service_flag(&state_dir).unwrap().is_some());

        pipes::client_send(&names.client_to_server, r#"{"cmd":"hi"}"#, 1.0).unwrap();
        let echoed = pipes::client_receive(&names.server_to_client, 1.0).unwrap();
        assert!(echoed.contains(r#""echo""#), "got: {echoed}");

        pipes::client_send(&names.client_to_server, r#"{"cmd":"shutdown"}"#, 1.0).unwrap();
        loop_thread.join().unwrap().unwrap();

        // Shutdown cleaned up the profile and the flag.
        assert!(state::connection_profile_paths(&state_dir).unwrap().is_empty());
        assert!(state::read_service_flag(&state_dir).unwrap().is_none());

        let _ = std::fs::remove_dir_all(stadhouder_dir.parent().unwrap());
    }

    /// An engine that logs every closed connection it's told about (id +
    /// reason), every init/shutdown hook call, and every timer that
    /// fires, to shared logs the test can inspect. Understands: "close_me"
    /// (orders the sending connection closed), "shutdown" (ends the run),
    /// "set_timer" ({"timer_id":..,"delay_ms":..}), and "cancel_timer"
    /// ({"timer_id":..}) - enough to drive the lifecycle-hook and
    /// close-reason tests below without dragging in TestEngine's unrelated
    /// behaviour.
    #[derive(Clone, Default)]
    struct RecordingEngine {
        close_log: Arc<Mutex<Vec<(ConnectionId, CloseReason)>>>,
        init_calls: Arc<Mutex<Vec<i64>>>,
        shutdown_calls: Arc<Mutex<Vec<i64>>>,
        fired_timers: Arc<Mutex<Vec<TimerId>>>,
    }

    impl StadhouderStateEngine for RecordingEngine {
        fn init(&mut self, timestamp_ms: i64) {
            self.init_calls.lock().unwrap().push(timestamp_ms);
        }

        fn process_messages(
            &mut self,
            timestamp_ms: i64,
            _memory_pct: f64,
            _new_connections: &[NewConnection],
            closed_connections: &[ClosedConnection],
            messages: &[InboundMessage],
            expired_timers: &[ExpiredTimer],
        ) -> EngineOutput {
            let mut close_log = self.close_log.lock().unwrap();
            for closed in closed_connections {
                close_log.push((closed.connection_id.clone(), closed.reason));
            }
            drop(close_log);

            let mut fired = self.fired_timers.lock().unwrap();
            for timer in expired_timers {
                fired.push(timer.timer_id.clone());
            }
            drop(fired);

            let mut output = EngineOutput::default();
            for message in messages {
                match message.message["cmd"].as_str().unwrap_or("") {
                    "close_me" => output.close_connections.push(message.connection_id.clone()),
                    "shutdown" => output.shutdown = true,
                    "set_timer" => {
                        let timer_id = message.message["timer_id"].as_str().unwrap_or("").to_string();
                        let delay_ms = message.message["delay_ms"].as_i64().unwrap_or(0);
                        output.timers.push(TimerRequest {
                            timer_id,
                            expiry_ms: timestamp_ms + delay_ms,
                            payload: Value::Null,
                        });
                    }
                    "cancel_timer" => {
                        let timer_id = message.message["timer_id"].as_str().unwrap_or("").to_string();
                        output.cancel_timers.push(timer_id);
                    }
                    _ => {}
                }
            }
            output
        }

        fn shutdown(&mut self, timestamp_ms: i64) {
            self.shutdown_calls.lock().unwrap().push(timestamp_ms);
        }
    }

    fn connect(stadhouder_dir: &Path, state_dir: &Path, connection_id: &str, user_id: &str) -> PipeNames {
        let names = pipes::pipe_names_for(stadhouder_dir, connection_id);
        ConnectionProfile {
            connection_id: connection_id.to_string(),
            user_id: user_id.to_string(),
            client_to_server_pipe: names.client_to_server.clone(),
            server_to_client_pipe: names.server_to_client.clone(),
            established_ms: time::calendar_now_millis(),
            established_sim_ms: None,
        }
        .save(state_dir)
        .unwrap();
        names
    }

    fn wait_for_flag(state_dir: &Path) {
        for _ in 0..100 {
            if state::read_service_flag(state_dir).unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("service flag never appeared");
    }

    /// Waits (bounded) for `connection_id` to show up in `log` and returns
    /// its recorded reason.
    fn wait_for_close_reason(
        log: &Mutex<Vec<(ConnectionId, CloseReason)>>,
        connection_id: &str,
        max_tries: u32,
    ) -> Option<CloseReason> {
        for _ in 0..max_tries {
            if let Some((_, reason)) = log
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| id == connection_id)
            {
                return Some(*reason);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    /// A close the engine orders (EngineOutput::close_connections) is
    /// actioned at once, but can only be REPORTED back on the following
    /// process_messages call - this proves that hand-off carries the
    /// right id and the EngineRequested reason.
    #[test]
    fn engine_requested_close_is_reported_next_call() {
        let stadhouder_dir = temp_stadhouder_dir("close-engine");
        let state_dir = stadhouder_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut engine = RecordingEngine::default();
        let close_log = Arc::clone(&engine.close_log);
        let loop_dir = stadhouder_dir.clone();
        let loop_thread = std::thread::spawn(move || {
            run_loop(&mut engine, &Config { test_env: false, ..Default::default() }, &loop_dir)
        });
        wait_for_flag(&state_dir);

        // "control" stays untouched until the very end, purely so there's
        // a connection left to ask the run to shut down.
        let control = connect(&stadhouder_dir, &state_dir, "30000000-0000-4000-8000-000000000002", "control");
        let subject_id = "30000000-0000-4000-8000-000000000001";
        let subject = connect(&stadhouder_dir, &state_dir, subject_id, "subject");

        pipes::client_send(&subject.client_to_server, r#"{"cmd":"close_me"}"#, 1.0).unwrap();

        assert_eq!(
            wait_for_close_reason(&close_log, subject_id, 200),
            Some(CloseReason::EngineRequested)
        );
        assert!(!state::delete_connection_profile(&state_dir, subject_id).unwrap());

        pipes::client_send(&control.client_to_server, r#"{"cmd":"shutdown"}"#, 1.0).unwrap();
        loop_thread.join().unwrap().unwrap();

        let _ = std::fs::remove_dir_all(stadhouder_dir.parent().unwrap());
    }

    /// A profile deleted out from under a live connection (what the client
    /// CGI program's `close` action does) is discovered on the service's
    /// next scan and reported as ClientDisconnected.
    #[test]
    fn client_disconnected_close_is_reported() {
        let stadhouder_dir = temp_stadhouder_dir("close-client");
        let state_dir = stadhouder_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut engine = RecordingEngine::default();
        let close_log = Arc::clone(&engine.close_log);
        let loop_dir = stadhouder_dir.clone();
        let loop_thread = std::thread::spawn(move || {
            run_loop(&mut engine, &Config { test_env: false, ..Default::default() }, &loop_dir)
        });
        wait_for_flag(&state_dir);

        let control = connect(&stadhouder_dir, &state_dir, "40000000-0000-4000-8000-000000000002", "control");
        let subject_id = "40000000-0000-4000-8000-000000000001";
        let subject = connect(&stadhouder_dir, &state_dir, subject_id, "subject");

        // Prime: blocks until the service has actually picked this
        // connection up (its pipes now exist). Deleting the profile
        // before that would vanish it before it was ever "live" in the
        // service's connection map, leaving nothing to report as closed.
        pipes::client_send(&subject.client_to_server, r#"{"cmd":"noop"}"#, 1.0).unwrap();

        state::delete_connection_profile(&state_dir, subject_id).unwrap();

        // The scan runs every four (real, at factor 1) seconds - generous
        // budget to cross at least one.
        assert_eq!(
            wait_for_close_reason(&close_log, subject_id, 240),
            Some(CloseReason::ClientDisconnected)
        );

        pipes::client_send(&control.client_to_server, r#"{"cmd":"shutdown"}"#, 1.0).unwrap();
        loop_thread.join().unwrap().unwrap();

        let _ = std::fs::remove_dir_all(stadhouder_dir.parent().unwrap());
    }

    /// init() fires exactly once, right at the start; shutdown() fires
    /// exactly once, at the "no connections left" idle-exit - and only
    /// there, never for an engine-ordered shutdown (covered by the two
    /// tests above, whose RecordingEngine.shutdown_calls stay empty
    /// despite each ending the run via EngineOutput::shutdown). No
    /// connections are ever made here, so with TIME_FACTOR cranked way up
    /// the 56-second idle-exit - and thus shutdown() - arrives almost at
    /// once; run_loop can just be called directly rather than polled from
    /// another thread.
    #[test]
    fn init_and_shutdown_hooks_fire_once_each() {
        let stadhouder_dir = temp_stadhouder_dir("init-shutdown");
        let state_dir = stadhouder_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("time_factor"), "1000").unwrap();
        // The only test in this crate touching DOCUMENT_ROOT (env vars are
        // process-global) - see full_connection_lifecycle's sibling note
        // in common::time for the same convention.
        std::env::set_var("DOCUMENT_ROOT", stadhouder_dir.parent().unwrap().join("public_html"));

        let mut engine = RecordingEngine::default();
        let init_calls = Arc::clone(&engine.init_calls);
        let shutdown_calls = Arc::clone(&engine.shutdown_calls);

        run_loop(&mut engine, &Config { test_env: true, ..Default::default() }, &stadhouder_dir).unwrap();

        assert_eq!(init_calls.lock().unwrap().len(), 1, "init() must fire exactly once");
        assert_eq!(shutdown_calls.lock().unwrap().len(), 1, "shutdown() must fire exactly once");
        assert!(
            init_calls.lock().unwrap()[0] <= shutdown_calls.lock().unwrap()[0],
            "init() must fire before shutdown()"
        );

        std::env::remove_var("DOCUMENT_ROOT");
        let _ = std::fs::remove_dir_all(stadhouder_dir.parent().unwrap());
    }

    /// A timer cancelled before it expires never fires - and a still-live
    /// connection can be told about neither its absence nor its presence
    /// except by NOT seeing a "timer_fired" reply where one would
    /// otherwise land; here that's checked directly against
    /// RecordingEngine's fired_timers log instead.
    #[test]
    fn cancelled_timer_never_fires() {
        let stadhouder_dir = temp_stadhouder_dir("timer-cancel");
        let state_dir = stadhouder_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut engine = RecordingEngine::default();
        let fired_timers = Arc::clone(&engine.fired_timers);
        let loop_dir = stadhouder_dir.clone();
        let loop_thread = std::thread::spawn(move || {
            run_loop(&mut engine, &Config { test_env: false, ..Default::default() }, &loop_dir)
        });
        wait_for_flag(&state_dir);

        let control = connect(&stadhouder_dir, &state_dir, "50000000-0000-4000-8000-000000000001", "control");

        pipes::client_send(
            &control.client_to_server,
            r#"{"cmd":"set_timer","timer_id":"t1","delay_ms":600}"#,
            1.0,
        )
        .unwrap();
        // A real gap before cancelling - a previously scheduled timer
        // being cancelled in a later call, not the same batch as the set
        // (that's a documented, distinct interaction: cancellation is
        // applied before a batch's own new timers are added, so a
        // reused id there reschedules rather than self-erasing).
        std::thread::sleep(Duration::from_millis(300));
        pipes::client_send(
            &control.client_to_server,
            r#"{"cmd":"cancel_timer","timer_id":"t1"}"#,
            1.0,
        )
        .unwrap();

        // Wait well past the timer's original expiry - it must never
        // appear in the fired-timers log.
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            !fired_timers.lock().unwrap().contains(&"t1".to_string()),
            "a cancelled timer fired anyway"
        );

        pipes::client_send(&control.client_to_server, r#"{"cmd":"shutdown"}"#, 1.0).unwrap();
        loop_thread.join().unwrap().unwrap();

        let _ = std::fs::remove_dir_all(stadhouder_dir.parent().unwrap());
    }
}
