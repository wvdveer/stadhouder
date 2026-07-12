//! The stadhouder service library. An application built on stadhouder
//! provides the *engine* - a concrete [`StadhouderStateEngine`]
//! implementation holding the application's state and logic - and hands it
//! to [`stadhouder_run`], which owns everything else: it records the run
//! (for the status endpoint), binds the named pipe the client CGI program
//! talks over, keeps the connection registry and timer list, and calls
//! [`StadhouderStateEngine::process_messages`] with each batch of events.
//! When `stadhouder_run` returns (the engine set
//! [`EngineOutput::shutdown`]), the application terminates.
//!
//! The application's service program deploys to `~/stadhouder/bin/`,
//! outside the web root - it is not a CGI endpoint and must never be
//! reachable over HTTP. Cron starts it every minute; like the CGI binaries
//! it locates cfg/ and state/ via DOCUMENT_ROOT's parent, which the cron
//! line must provide:
//!   * * * * * DOCUMENT_ROOT=$HOME/public_html $HOME/stadhouder/bin/<app>
//!
//! Instances coordinate through a service flag file: a starting instance
//! that finds a fresh flag terminates at once (the previous start is still
//! operating); otherwise it takes over, and exits again when it has had no
//! client connections for its first 56 seconds - so exactly one instance
//! runs while there's work, and none linger when there isn't. The runtime
//! is strictly single-threaded (a loop-and-sleep); see `runtime` for the
//! full lifecycle.
use common::{layout, state, time, Config};

mod runtime;

/// A connection id: a UUID minted by the library when a client connects.
pub type ConnectionId = String;

/// A client connection that appeared since the last `process_messages`
/// call.
pub struct NewConnection {
    pub connection_id: ConnectionId,
    /// Meaningful to the specific state engine (e.g. the identity the
    /// client presented); the library treats it as opaque.
    pub user_id: String,
}

/// A message that arrived from a connection since the last
/// `process_messages` call.
pub struct InboundMessage {
    pub connection_id: ConnectionId,
    /// JSON meaningful to the state engine.
    pub message: serde_json::Value,
}

/// Why a connection appearing in `closed_connections` was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The client formally disconnected: its `close` request deleted the
    /// connection's profile file (or the file otherwise vanished).
    ClientDisconnected,
    /// The service disconnected it after two minutes with no message from
    /// the client (not even a heartbeat).
    ServerIdleTimeout,
    /// The engine itself ordered this close, via a previous call's
    /// `EngineOutput::close_connections`. Reported one call later than the
    /// request: the connection was already torn down by the time that
    /// call returned, so it couldn't appear in that same call's own
    /// `closed_connections`.
    EngineRequested,
}

impl CloseReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CloseReason::ClientDisconnected => "client_disconnected",
            CloseReason::ServerIdleTimeout => "server_idle_timeout",
            CloseReason::EngineRequested => "engine_requested",
        }
    }
}

/// A connection that was closed since the last `process_messages` call,
/// and why.
pub struct ClosedConnection {
    pub connection_id: ConnectionId,
    pub reason: CloseReason,
}

/// A timer id: chosen by the engine when it sets a timer (see
/// [`TimerRequest::timer_id`]), opaque to the library - just a handle the
/// engine can later hand back in [`EngineOutput::cancel_timers`] to cancel
/// that timer, or expect echoed back in [`ExpiredTimer::timer_id`] when it
/// fires.
pub type TimerId = String;

/// A timer that expired since the last `process_messages` call.
pub struct ExpiredTimer {
    /// The id it was given when set.
    pub timer_id: TimerId,
    /// The expiry time the timer was set for (engine time, ms since the
    /// Unix epoch - simulated time on a test instance).
    pub expiry_ms: i64,
    /// The JSON the state engine gave when it set the timer.
    pub payload: serde_json::Value,
}

/// A message for the library to deliver to one connection.
pub struct OutboundMessage {
    pub connection_id: ConnectionId,
    pub message: serde_json::Value,
}

/// A timer for the library to set.
pub struct TimerRequest {
    /// An id of the engine's own choosing, naming this timer for a later
    /// [`EngineOutput::cancel_timers`] and echoed back in
    /// [`ExpiredTimer::timer_id`] when it fires.
    pub timer_id: TimerId,
    /// When to fire (engine time, ms since the Unix epoch - simulated time
    /// on a test instance).
    pub expiry_ms: i64,
    /// Handed back untouched in [`ExpiredTimer::payload`] when it fires.
    pub payload: serde_json::Value,
}

/// Everything the engine wants done after processing a batch.
#[derive(Default)]
pub struct EngineOutput {
    /// Connections to be closed by the server. Messages queued for them in
    /// this same batch are still delivered with the close.
    pub close_connections: Vec<ConnectionId>,
    /// Messages to send to clients.
    pub messages: Vec<OutboundMessage>,
    /// Timers to be set.
    pub timers: Vec<TimerRequest>,
    /// Previously scheduled timers to cancel, by the id given in their
    /// `TimerRequest::timer_id`. A cancelled timer never fires - it's
    /// removed without appearing in any future `expired_timers`. Applied
    /// before this same batch's `timers` are added, so setting a new timer
    /// with an id also present here replaces the old one rather than
    /// immediately cancelling itself. An id that doesn't match any pending
    /// timer (already fired, or never existed) is silently ignored.
    pub cancel_timers: Vec<TimerId>,
    /// End the service run: [`stadhouder_run`] returns after this batch's
    /// replies are delivered, and the application terminates. (Cron starts
    /// the next run.)
    pub shutdown: bool,
}

/// The application's side of the contract. The library calls this with
/// everything that happened since the last call - strictly from one thread,
/// so the engine never needs its own locking.
///
/// Parameters:
/// - `timestamp_ms` - the current engine time, ms since the Unix epoch
///   (simulated time on a test instance);
/// - `new_connections` - connections that appeared since the last call;
/// - `closed_connections` - connections closed since the last call, each
///   tagged with why (see [`CloseReason`]);
/// - `messages` - messages that arrived from connections since the last
///   call (heartbeats are filtered out before this point - they never
///   reach the engine, see `common::heartbeat`);
/// - `expired_timers` - timers that expired since the last call.
pub trait StadhouderStateEngine {
    /// Called once, by the service, at the very start of the run - before
    /// anything else, including the first `process_messages` call. Only
    /// called by the instance that actually takes over as the live
    /// service (one that defers to an already-live instance never calls
    /// this). Default: does nothing.
    fn init(&mut self, timestamp_ms: i64) {
        let _ = timestamp_ms;
    }

    fn process_messages(
        &mut self,
        timestamp_ms: i64,
        new_connections: &[NewConnection],
        closed_connections: &[ClosedConnection],
        messages: &[InboundMessage],
        expired_timers: &[ExpiredTimer],
    ) -> EngineOutput;

    /// Called once, by the service, right before it terminates because it
    /// found no client connections left (the 56-second idle-exit - see the
    /// module doc comment). NOT called for an engine-ordered shutdown
    /// (`EngineOutput::shutdown`) - the engine already knows about that,
    /// having just requested it. Default: does nothing.
    fn shutdown(&mut self, timestamp_ms: i64) {
        let _ = timestamp_ms;
    }
}

/// The service program's whole life: runs for as long as needed, processing
/// the client messages back and forth between the client CGI program and
/// `engine`, and returns once the engine sets [`EngineOutput::shutdown`].
pub fn stadhouder_run(engine: &mut dyn StadhouderStateEngine) -> Result<(), String> {
    let config = Config::load()?;
    state::write_last_run_ms(time::now_millis(&config)?)?;

    runtime::run_loop(engine, &config, &layout::stadhouder_dir()?)
}
