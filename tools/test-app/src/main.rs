//! The test harness's stand-in for a real application built on stadhouder:
//! it defines a concrete StadhouderStateEngine and hands it to
//! stadhouder_run, exactly as a using application would. Deployed by
//! copy-to-site.sh to site/stadhouder/bin/ and started (in cron's place)
//! by cron-sim.
//!
//! The engine is deliberately tiny but exercises every part of the
//! process_messages contract. It keeps one shared counter (so tests can
//! prove state survives across separate client requests and connections)
//! and a roster of live connections. Commands (as {"cmd": ...} messages):
//!   {"cmd":"add","n":N}                    -> counter += N, replies {"counter":...}
//!   {"cmd":"get"}                          -> replies {"counter":...}
//!   {"cmd":"broadcast","text":T}           -> sends {"broadcast":T} to every connection
//!   {"cmd":"set_timer","delay_ms":D,"tag":T} -> timer at now+D, id "<connection>:<tag>";
//!                                             on expiry the connection gets {"timer_fired":T,...}
//!   {"cmd":"cancel_timer","tag":T}         -> cancels that same timer if still pending;
//!                                             replies {"timer_cancelled":T}
//!   {"cmd":"close_me"}                     -> asks the server to close this connection
//!   {"cmd":"shutdown"}                     -> replies {"bye":true} and ends the run
//! New connections are greeted with {"hello":<user_id>}. Whenever a
//! connection closes (for any reason), every other still-live connection
//! is sent {"connection_closed":{"connection_id":..,"reason":..}} - this
//! is how the system tests observe close reasons end-to-end, since
//! heartbeats and close bookkeeping otherwise happen invisibly at the
//! transport level.
//!
//! init/shutdown also each drop a marker file (state/engine_init_marker,
//! state/engine_shutdown_marker - the engine time they fired at) purely so
//! the system tests can observe that the library actually called them;
//! nothing about the marker files themselves is part of the stadhouder
//! contract.
use serde_json::{json, Value};
use stadhouder::{
    stadhouder_run, ClosedConnection, ConnectionId, EngineOutput, ExpiredTimer, InboundMessage,
    NewConnection, OutboundMessage, StadhouderStateEngine, TimerRequest,
};

struct TestEngine {
    counter: i64,
    roster: Vec<ConnectionId>,
}

impl TestEngine {
    fn reply(&self, output: &mut EngineOutput, connection_id: &str, message: Value) {
        output.messages.push(OutboundMessage {
            connection_id: connection_id.to_string(),
            message,
        });
    }

    fn write_marker(name: &str, timestamp_ms: i64) {
        if let Ok(dir) = common::state::state_dir() {
            let _ = std::fs::write(dir.join(name), timestamp_ms.to_string());
        }
    }
}

impl StadhouderStateEngine for TestEngine {
    fn init(&mut self, timestamp_ms: i64) {
        eprintln!("test-app: engine init at {timestamp_ms}");
        Self::write_marker("engine_init_marker", timestamp_ms);
    }

    fn process_messages(
        &mut self,
        timestamp_ms: i64,
        _memory_pct: f64,
        new_connections: &[NewConnection],
        closed_connections: &[ClosedConnection],
        messages: &[InboundMessage],
        expired_timers: &[ExpiredTimer],
    ) -> EngineOutput {
        let mut output = EngineOutput::default();

        for connection in new_connections {
            self.roster.push(connection.connection_id.clone());
            self.reply(
                &mut output,
                &connection.connection_id,
                json!({"hello": connection.user_id}),
            );
        }

        for closed in closed_connections {
            self.roster.retain(|id| id != &closed.connection_id);
            for id in &self.roster {
                output.messages.push(OutboundMessage {
                    connection_id: id.clone(),
                    message: json!({
                        "connection_closed": {
                            "connection_id": closed.connection_id,
                            "reason": closed.reason.as_str(),
                        }
                    }),
                });
            }
        }

        for inbound in messages {
            let connection_id = &inbound.connection_id;
            match inbound.message["cmd"].as_str().unwrap_or("") {
                "add" => {
                    self.counter += inbound.message["n"].as_i64().unwrap_or(0);
                    self.reply(&mut output, connection_id, json!({"counter": self.counter}));
                }
                "get" => {
                    self.reply(&mut output, connection_id, json!({"counter": self.counter}));
                }
                "broadcast" => {
                    let text = inbound.message["text"].clone();
                    for id in &self.roster {
                        output.messages.push(OutboundMessage {
                            connection_id: id.clone(),
                            message: json!({"broadcast": text}),
                        });
                    }
                }
                "set_timer" => {
                    let delay_ms = inbound.message["delay_ms"].as_i64().unwrap_or(0);
                    let tag = inbound.message["tag"].clone();
                    output.timers.push(TimerRequest {
                        timer_id: format!("{connection_id}:{tag}"),
                        expiry_ms: timestamp_ms + delay_ms,
                        payload: json!({
                            "tag": tag,
                            "connection_id": connection_id,
                        }),
                    });
                    self.reply(&mut output, connection_id, json!({"timer_set": true}));
                }
                "cancel_timer" => {
                    let tag = inbound.message["tag"].clone();
                    output.cancel_timers.push(format!("{connection_id}:{tag}"));
                    self.reply(&mut output, connection_id, json!({"timer_cancelled": tag}));
                }
                "close_me" => {
                    self.roster.retain(|id| id != connection_id);
                    output.close_connections.push(connection_id.clone());
                    self.reply(&mut output, connection_id, json!({"goodbye": true}));
                }
                "shutdown" => {
                    self.reply(&mut output, connection_id, json!({"bye": true}));
                    output.shutdown = true;
                }
                other => {
                    self.reply(
                        &mut output,
                        connection_id,
                        json!({"error": format!("unknown command '{other}'")}),
                    );
                }
            }
        }

        for timer in expired_timers {
            if let Some(connection_id) = timer.payload["connection_id"].as_str() {
                self.reply(
                    &mut output,
                    connection_id,
                    json!({"timer_fired": timer.payload["tag"], "expiry_ms": timer.expiry_ms}),
                );
            }
        }

        output
    }

    fn shutdown(&mut self, timestamp_ms: i64) {
        eprintln!("test-app: engine shutdown at {timestamp_ms}");
        Self::write_marker("engine_shutdown_marker", timestamp_ms);
    }
}

fn main() {
    let mut engine = TestEngine {
        counter: 0,
        roster: Vec::new(),
    };
    // When stadhouder_run finishes (the engine ordered shutdown, or the
    // service found no connections left and called shutdown()), the
    // application terminates.
    if let Err(e) = stadhouder_run(&mut engine) {
        eprintln!("test-app: {e}");
        std::process::exit(1);
    }
}
