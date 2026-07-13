# stadhouder

Stadhouder is an engine for running a *stateful service* on cheap shared
hosting (a cPanel account): no SSH, no daemons. An application built on
stadhouder implements the `StadhouderStateEngine` trait (its state and
logic) and hands that object to the `stadhouder` library's
`stadhouder_run`, which does everything else: it runs for as long as
needed, processing client messages back and forth between the client
cgi-bin program and the engine object; when it finishes (the engine orders
shutdown), the application terminates.

Besides `process_messages`, `StadhouderStateEngine` has two lifecycle
hooks, both optional (default: do nothing) and each called at most once:
`init(timestamp_ms)`, right after this instance confirms it's the live
one (never called by one that defers to an already-live instance), and
`shutdown(timestamp_ms)`, right before the service terminates because it
found no client connections left (the 56-second idle-exit) - NOT called
for an engine-ordered shutdown (`EngineOutput::shutdown`), since the
engine already knows about that, having just requested it.

The engine's main work happens in `process_messages`, called with each
batch of events - the current timestamp (simulated time on a test
instance), the service's own memory usage as a percentage of
`MEMORY_LIMIT_MB` (`0.0` when unset - see [Configuration](#configuration)),
new connections (connection id + a user id string meaningful to the
engine), connections closed since the last call, messages that arrived
from connections (connection id + JSON, with heartbeats already filtered
out - see below), and timers that expired (expiry time + the JSON the
engine gave when setting them). It returns connections to be closed by the
server, messages to send to clients, timers to be set, timer ids to
*cancel* (see below), and (to end the run) a shutdown flag. Calls arrive
strictly one at a time, so engines never need their own locking.

Every timer the engine sets carries an id of its own choosing
(`TimerRequest::timer_id`), echoed back in `ExpiredTimer::timer_id` when it
fires. `EngineOutput::cancel_timers` names ids to cancel - a cancelled
timer is removed without ever firing. Cancellation is applied before that
same batch's own new timers are added, so an engine can "reschedule" a
timer by cancelling and re-setting the same id in one call; cancelling and
setting the *same* id in one call, for a timer that didn't already exist,
is a no-op for the cancel (nothing to cancel yet) followed by the set
taking effect.

Each closed connection is tagged with why: `ClientDisconnected` (the
client's own `close` request, or its profile file otherwise vanishing),
`ServerIdleTimeout` (no message and no served `poll` from the client for
two minutes), or
`EngineRequested` (the engine itself ordered the close via
`EngineOutput::close_connections` on a *previous* call - a close can only
be reported to the call *after* the one that requested it, since the
connection is already gone by the time that call's output comes back).

Connections are file-driven. When a client requests one, the client CGI
program mints the connection uuid and writes a *connection profile* into
`stadhouder/state/` - the uuid, the user_id string, the two pipe names,
the calendar time established, and (when `TEST_ENV=true`) the simtime
established. Every filename derived from a connection (the profile, the
pipe names) uses the uuid with underscores replacing dashes. The service
scans the state directory (at least once a second): a new profile becomes
a `NewConnection`; a vanished profile means the client closed (deleting
the profile is exactly what the CGI `close` request does); when the engine
orders a close, the service deletes the profile itself. Clients drive
everything through **one** client CGI program, naming the action with a
`"kind"` field in the JSON request body: `status` (health/readiness -
service name, version, `test_env`, current engine time, when the
service last ran, and its last-measured memory usage) / `connect` / `send` (one frame down the connection's
client-to-server pipe) / `poll` / `close`.

If `COOKIE_NAME` is configured, `connect` requires a cookie of that name
on the request - presence only, never its value - before it will mint a
connection. If `USER_ID_URL` (and `USER_ID_JSON`) are also configured,
connect goes further: it calls that same-host URL, forwarding the
request's own Cookie header, and reads the *verified* identity out of the
JSON response at the `USER_ID_JSON` field path - replacing whatever
user_id the request itself claimed. If `MEMORY_LIMIT_MB` is configured,
connect also refuses once the service's last-measured memory usage
reaches `MEMORY_REJECT_PCT` of that limit (default 100%). See
[Configuration](#configuration).

`poll` takes a `wait_ms` (engine time, scaled by `TIME_FACTOR`): it blocks
for up to that long, returning the moment a message is available. If the
wait elapses with nothing, it sends a *heartbeat* message to the service
first, then returns an empty array - the heartbeat refreshes the
connection's last-communication time (keeping it under the two-minute
inactivity disconnect) without ever reaching the state engine; the service
filters it out before building a `process_messages` batch.

Two executable surfaces result:

- the **client CGI program** (`public_html/cgi-bin/stadhouder/client`),
  run per-request by the web server - it relays one message to the running
  service and returns the engine's reply;
- the **application's service program** (in `~/stadhouder/bin/`, outside
  the web root), built on the `stadhouder` library. cPanel can't host
  long-running processes, so cron starts it every minute:

```
* * * * * DOCUMENT_ROOT=$HOME/public_html $HOME/stadhouder/bin/<app>
```

Locally, `tools/cron-sim` stands in for that cron line - started by
`start-site.sh`, it fires the staged service executable every *simulated*
minute (scaled by `TIME_FACTOR`, like every other wait) instead of every
real one, so a test can compress a long cron-driven lifecycle into a few
real seconds. Like real cron it never waits for the previous tick's
instance to finish - most ticks fire into an already-live singleton and
that instance exits almost immediately.

The service is strictly single-threaded (a loop and sleep), and instances
coordinate through a *service flag file* in `stadhouder/state/`: a starting
instance that finds a flag fresher than two minutes terminates at once (the
previous start is still operating); otherwise it writes its own flag and
refreshes it every twenty seconds while it runs, alongside its own
memory usage as a percentage of `MEMORY_LIMIT_MB` (see
[Configuration](#configuration)) - `connect` reads that percentage to
refuse new connections once it's too high. It scans for client
connections every four seconds; with none 56 seconds after start it removes
its flag and exits (the next cron start takes over), and while connections
exist it serves them until all are closed. It disconnects a client itself -
removing the connection's profile file and pipes - when the engine orders
it, and also when the client has sent no message and had no `poll` served
for two minutes. All these
durations are engine time (they scale with `TIME_FACTOR`).

The two talk over **named pipes** - one client-to-server / server-to-client
pair per connection, named with the connection uuid (underscores for
dashes): on Windows `\\.\pipe\stadhouder\{c2s,s2c}_<uuid>`, on Linux FIFOs
created by the service in `~/stadhouder/pipes/`. Framing is a 4-byte length
prefix + UTF-8 payload.

It shares its architecture and tooling conventions with
[KeyScarf](https://github.com/wvdveer/keyscarf). Stadhouder does not do user
identity itself: it is designed to sit behind an identity provider. The test
harness uses KeyScarf as that provider (installed from its release package,
exactly as a real admin would - including its own MySQL database, set up
through its setup wizard), but the final product should work with other
providers too. Stadhouder has no database of its own.

## Layout

```
rust/                 The product (KeyScarf's `authsite/` equivalent)
└── backend/          Cargo workspace: `common` (config/state/time/pipes
                      foundations), `api` (the CGI endpoint binaries) and
                      `service` (the `stadhouder` library that using
                      applications build their service program on)

tools/                Development-only tooling; nothing here ships
├── cgi-fileserver/   Local dev server: static files + real CGI execution,
│                     as strict about CGI output as Apache (from KeyScarf)
├── cron-sim/         The harness's stand-in for cron - started by
│                     start-site.sh, fires the staged service executable
│                     every SIMULATED minute (scaled by TIME_FACTOR),
│                     exactly as a real cron line fires it every real minute
├── test-app/         The harness's stand-in for a real application using
│                     the stadhouder library - a small engine exercising the
│                     whole process_messages contract (shared counter,
│                     broadcasts, timers, closes); cron-sim runs it as the
│                     service program
├── scripts/          Build/deploy/test glue (see below)
└── downloads/        Fetched release archives (gitignored)

dev_env/              Per-developer config overrides (only *.example committed)
site/                 Local staging area mirroring a real cPanel account
                      (gitignored; assembled by the scripts)
docs/                 Guides for people building on or operating stadhouder -
                      see `docs/administrators-guide.md` for installing and
                      running a deployed application
package/              Release archives built by package.sh (gitignored)
```

The `site/` tree mirrors a deployed cPanel account: `public_html/` is the
web root (with `cgi-bin/stadhouder/` and, when staged, `cgi-bin/keyscarf/`),
while `stadhouder/cfg/`, `stadhouder/state/`, `stadhouder/bin/` and
KeyScarf's `keyscarf/` folder (cfg/, db/, images/) sit *outside* it -
anything under the web root is directly downloadable over plain HTTP. All
stadhouder executables locate cfg/ and state/ via `DOCUMENT_ROOT`'s parent,
in both local dev and production.

## Configuration

Stadhouder reads `stadhouder/cfg/stadhouder.conf` (a `KEY=value` file, found
via `DOCUMENT_ROOT`'s parent - `site/stadhouder/cfg/stadhouder.conf`
locally). A missing file is a valid production configuration; every key has
a safe default:

| Key             | Default | Meaning                                                       |
|-----------------|---------|------------------------------------------------------------------|
| `TEST_ENV`      | `false` | True if this is a test instance of stadhouder                    |
| `COOKIE_NAME`   | unset   | If set, `connect` requires a cookie of this name on the request (presence only - the value is never inspected) before it will mint a connection. Locally set to `keyscarf_session` |
| `USER_ID_URL`   | unset   | If set, `connect` calls this same-host path, forwarding the request's Cookie header, to verify who's actually calling (see `USER_ID_JSON`) instead of trusting the request's self-asserted user_id. Locally set to `/cgi-bin/keyscarf/api_session_check` |
| `USER_ID_JSON`  | unset   | The dot-separated field path (e.g. `data.id`) to read the verified id from in `USER_ID_URL`'s JSON response - stadhouder assumes nothing else about that response's shape. Required whenever `USER_ID_URL` is set. Locally set to `data.id` |
| `MEMORY_LIMIT_MB` | unset | If set, the service measures its own memory (RSS) against this ceiling and writes the usage percentage (`memory_pct`) into its service flag file every 20 seconds. Unset means no limit - the service never measures, and `memory_pct` stays `0.0` |
| `MEMORY_REJECT_PCT` | `100` | The `memory_pct` at or above which `connect` refuses new connections. Only meaningful when `MEMORY_LIMIT_MB` is also set |

Mutable runtime state lives separately, as files under `stadhouder/state/`
(stadhouder has no database). Currently:

| State file          | Default | Meaning                                                     |
|---------------------|---------|-------------------------------------------------------------|
| `sim_time_diff`     | `0`     | Simulation time minus true calendar time, in milliseconds   |
| `time_factor`       | `1`     | The rate simulation time passes at relative to calendar time (must be > 0; fractions slow it down) |
| `time_factor_start` | `0`     | The calendar-time anchor (ms) the rate applies from         |
| `last_run`          | absent  | Engine time (ms) of the service's most recent cron run      |
| `service_flag`      | absent  | JSON object (`updated_ms`, `memory_pct`) the live service instance last wrote; removed on exit. `connect` reads `memory_pct` against `MEMORY_REJECT_PCT` |
| `connection_*.json` | -       | One profile per live connection (see above); stale ones are purged when the service starts |

A test instance (`TEST_ENV=true`) runs on *simulated time*, which can be
shifted and can pass faster or slower than calendar time:

```
simtime = TIME_FACTOR_START + (caltime - TIME_FACTOR_START) * TIME_FACTOR + SIM_TIME_DIFF
```

Tests move an instance through time by rewriting these files at runtime.
Sleeps and waits in both the client CGI program and the service are
periods of *engine* time, divided by `TIME_FACTOR` to get the real wait -
at factor 5, a 250ms service sleep is a real 50ms, and a 30-second engine
timer fires in 6 real seconds. Engine code takes "now" from
`common::time`, never from the system clock directly; a production
instance always runs on true calendar time and never reads these files.

## Testing

- **Unit tests** - `cargo test` in `rust/backend`.
- **`tools/scripts/system-test.sh`** - curl-driven HTTP assertions against
  the real CGI binaries served by the local dev server. The KeyScarf
  (identity provider) checks run only when KeyScarf is staged.

## What's in `tools/scripts/`

| Script              | Purpose                                                              |
|---------------------|----------------------------------------------------------------------|
| `build-tools.sh`    | Builds the dev tools (currently cgi-fileserver)                      |
| `fetch-keyscarf.sh` | Downloads the KeyScarf release archive for this host OS              |
| `stage-keyscarf.sh` | Unpacks the archive and overlays it onto `site/`                     |
| `copy-to-site.sh`   | Stages built stadhouder CGI binaries into `site/.../cgi-bin/stadhouder/` |
| `start-site.sh`     | Serves `site/public_html` at `http://127.0.0.1:8080`, and starts cron-sim |
| `system-test.sh`    | The curl-driven system test suite                                    |
| `package.sh`        | Builds release archives of stadhouder for a consuming application to install (see `docs/administrators-guide.md`) |

By default `fetch-keyscarf.sh` pulls the 0.1.0 release from GitHub
(`.../keyscarf/releases/download/0_1_0/keyscarf_<os>_0_1_0.<ext>`); create
`dev_env/keyscarf.conf` (from its `.example`) to point it at another
location, including a `file:///` URL for a locally built package.

## Development quick start

```sh
tools/scripts/build-tools.sh        # dev server + test application

cd rust/backend && cargo build && cd ../..   # the CGI endpoint binaries
tools/scripts/copy-to-site.sh rust/backend/target/debug/*.exe tools/test-app/target/debug/*.exe

# For identity-provider work: install KeyScarf into the site. Its wizard
# (at /keyscarf/setup/setup.html once the site is up) needs a disposable
# MySQL database to point it at.
tools/scripts/fetch-keyscarf.sh
tools/scripts/stage-keyscarf.sh

tools/scripts/start-site.sh         # serve http://127.0.0.1:8080

tools/scripts/system-test.sh        # in another terminal
```

Smoke check: `curl --data '{"kind":"status"}' http://127.0.0.1:8080/cgi-bin/stadhouder/client`
answers `{"ok":true,...,"data":{"service":"stadhouder",...}}`.
