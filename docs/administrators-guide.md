# stadhouder — Administrator's Guide

Stadhouder is an engine for running a *stateful service* on cheap shared
hosting (a cPanel account): no SSH, no daemons, no database of its own.
An application is built **on top of** stadhouder by a developer, who
compiles stadhouder's `common`/`service` crates into their own service
program; stadhouder itself ships as source for that purpose, plus a
pre-built CGI binary.

This guide is for the person who **installs and operates** a
stadhouder-based application on a real hosting account. It assumes the
application has already been built (that's the application developer's
job — see the project README and the application's own docs); this
guide covers getting it running on the server, keeping it running, and
diagnosing it when something looks wrong.

## The pieces

A deployed application built on stadhouder has two executable surfaces:

| Piece | Where it lives | What it does |
|---|---|---|
| **Client CGI program** | `public_html/cgi-bin/stadhouder/client` (web root) | Run by the web server on every request; relays one message to the running service and returns its reply. Ships pre-built — you never compile this yourself. |
| **Service program** | `~/stadhouder/bin/<app>` (outside the web root) | The application's own binary, built by its developer against the `stadhouder` library. Started by cron once a minute; runs for as long as clients are connected, then exits. |

Everything mutable — configuration and runtime state — lives in
`~/stadhouder/`, one level above the web root, so none of it is ever
directly downloadable over HTTP:

```
public_html/
└── cgi-bin/stadhouder/client      (the CGI binary)

stadhouder/                        (sibling of public_html/, NOT inside it)
├── cfg/stadhouder.conf            (configuration — see below)
├── state/                         (runtime state, connection profiles)
├── bin/<app>                      (the service program)
└── pipes/                         (Linux only — named pipes per connection)
```

Stadhouder locates `cfg/` and `state/` via `DOCUMENT_ROOT`'s parent, in
both the client CGI program and the service program — there is no other
configuration mechanism to point it at these directories.

## Installing the application

Stadhouder itself is distributed as a release archive
(`stadhouder_<os>_<version>.{tar.gz,zip}`, built by
`tools/scripts/package.sh`) containing:

```
src/common/     shared crate source (config/state/time/pipes foundations)
src/service/    the stadhouder library source
bin/client      the pre-built client CGI binary for that OS
```

Because Rust has no stable ABI for shipping pre-compiled libraries, the
`common` and `service` crates are shipped as **source**: the application
developer's build compiles them directly into the application's own
service binary. As the administrator, what you receive from the
application developer is:

1. The `client` CGI binary (either stadhouder's own pre-built one, staged
   as-is, or bundled into the application's own release package — check
   with your application's build process).
2. The compiled service binary, `<app>`.

To install:

1. Place `client` at `public_html/cgi-bin/stadhouder/client`.
2. Place `<app>` at `~/stadhouder/bin/<app>`.
3. Create `~/stadhouder/cfg/` and `~/stadhouder/state/` if they don't
   already exist (a fresh install has neither — see *Configuration*
   below for what a missing config file means).
4. **Make both binaries executable.** Depending on how the archive was
   extracted — cPanel's File Manager in particular — files can land
   without the execute bit set. Set `public_html/cgi-bin/stadhouder/client`
   and `~/stadhouder/bin/<app>` to `0755` (File Manager: select the file →
   Permissions → 755, or `chmod 755 <path>` with shell access). The
   tell-tale symptom of a missed step here is the client CGI program
   failing immediately, or `status` calls timing out, because the web
   server served an error page instead of running the binary. suEXEC
   hosts are strict in the other direction too: neither file, nor their
   parent directories, may be group- or world-writable, or Apache will
   refuse to run them.
5. Add the cron line (see below).

### The cron line

cPanel accounts can't host long-running processes, so cron stands in for
one, starting the service program every minute:

```
* * * * * DOCUMENT_ROOT=$HOME/public_html $HOME/stadhouder/bin/<app>
```

`DOCUMENT_ROOT` must be set explicitly on the cron line — cron gives
jobs a minimal environment, and stadhouder needs this variable to find
`stadhouder/cfg/` and `stadhouder/state/` as siblings of the web root.

Every tick starts a new instance of `<app>`, but only one is ever
actually live at a time: each starting instance checks
`stadhouder/state/service_flag` — if it was refreshed within the last
two minutes, a previous instance is still running, and the new one exits
immediately. Otherwise it takes over: writes its own flag (refreshing it
every 20 seconds while it runs), scans for client connections every four
seconds, and — if none show up within 56 seconds of starting — removes
its flag and exits, leaving the next cron tick to pick things up. While
connections exist, it keeps serving them for as long as they last,
regardless of how many cron ticks pass in the meantime.

This means: **don't** run the cron job more often than once a minute (it
won't help — only one instance is ever live), and **don't** be alarmed by
most ticks exiting within a second or two (they found a live instance and
deferred to it — this is the normal, expected case).

## Configuration

Stadhouder reads `stadhouder/cfg/stadhouder.conf` — a plain `KEY=value`
file. **A missing file is a valid, safe production configuration** —
every key defaults to its most restrictive/inert setting, so a fresh
install with no config file at all runs, just without cookie or
identity-provider enforcement.

| Key | Default | Meaning |
|---|---|---|
| `TEST_ENV` | `false` | Whether this is a test instance. **Never set this to `true` in production** — it switches the service onto simulated time driven by files in `state/` instead of the real clock (see *Security notes*). |
| `COOKIE_NAME` | unset | If set, a `connect` request must carry a cookie of this name — its *presence*, never its value — or the request is refused before a connection is minted. |
| `USER_ID_URL` | unset | If set, `connect` calls this same-host URL (forwarding the request's own `Cookie` header) to obtain the caller's *verified* identity, instead of trusting whatever `user_id` the request itself claims. Requires `USER_ID_JSON`. |
| `USER_ID_JSON` | unset | Dot-separated field path (e.g. `data.id`) locating the verified user id inside `USER_ID_URL`'s JSON response. Stadhouder assumes nothing else about that response's shape. |
| `MEMORY_LIMIT_MB` | unset | If set, the service measures its own memory (RSS) every 20 seconds and treats this as the ceiling. Unset means no measurement is taken at all and no memory-based rejection ever applies. |
| `MEMORY_REJECT_PCT` | `100` | The percentage of `MEMORY_LIMIT_MB` at or above which `connect` starts refusing new connections. Only meaningful when `MEMORY_LIMIT_MB` is also set; lower it (e.g. `90`) for headroom before the process is completely full. |

Configuration changes take effect on the **next service start** — there
is no long-lived process to signal, so a config edit made while the
service is already running (mid-connection) won't be picked up until it
next exits and a fresh instance starts.

### Guarding against memory exhaustion

Set `MEMORY_LIMIT_MB` to give the service a self-imposed memory ceiling —
useful on shared hosting where an account-wide memory cap means one
runaway application can take down everything else on it. Once set, the
service measures its own RSS every 20 seconds and writes the result (as a
percentage of the limit) into `stadhouder/state/service_flag`. `connect`
checks that percentage on every new-connection attempt and refuses once
it reaches `MEMORY_REJECT_PCT`, with the error `"service memory limit
exceeded"` — existing connections are unaffected; only new ones are
turned away.

This is a soft cap, not a hard process limit: the service is never
killed, and nothing stops its RSS climbing past the configured ceiling
between measurements. It buys time and turns a slow leak into a visible,
recoverable symptom (`connect` failures) instead of an OOM kill or a
host-level suspension — it is not a substitute for fixing the underlying
memory growth, and cPanel-level memory limits (if your host enforces
them) still apply on top of it regardless of this setting.

`status` reports the current figure as `service_memory_pct` (see
*Monitoring* below) - `null` when `MEMORY_LIMIT_MB` is unset, or when no
instance has run yet to measure it.

### Pairing with an identity provider

Stadhouder does no user identity itself — it expects to sit behind an
identity provider. The typical pairing is with
[KeyScarf](https://github.com/wvdveer/keyscarf), staged alongside it on
the same cPanel account:

```
COOKIE_NAME=keyscarf_session
USER_ID_URL=/cgi-bin/keyscarf/api_session_check
USER_ID_JSON=data.id
```

Any provider that exposes a same-host, cookie-authenticated "who is
this" endpoint returning JSON works the same way — `USER_ID_URL` and
`USER_ID_JSON` make no other assumption about the provider.

## Runtime state

Everything mutable lives as files under `stadhouder/state/` — stadhouder
has no database. You generally shouldn't need to touch these by hand in
production (they're written and cleaned up by the service itself), but
knowing what they are helps when diagnosing an issue:

| State file | Meaning |
|---|---|
| `service_flag` | JSON object the live service instance last wrote: the engine time (ms) it was refreshed at, and `memory_pct` (see `MEMORY_LIMIT_MB` under *Configuration*). Present only while an instance is running; its absence (or staleness beyond two minutes) is what lets the next cron tick take over. |
| `last_run` | Engine time (ms) of the service's most recent cron-triggered run. |
| `connection_*.json` | One profile per live client connection (uuid, user id, pipe names, connection time). Stale ones are purged automatically when the service starts. |
| `sim_time_diff`, `time_factor`, `time_factor_start` | Simulated-time controls — meaningful only when `TEST_ENV=true`. A production instance never reads these. |

If the service ever seems stuck (cron ticks keep exiting immediately, but
no client requests are being served), a stale `service_flag` from a
process that died uncleanly is the first thing to check — removing it
lets the next cron tick start a fresh instance. This should be rare: the
flag's own two-minute freshness check exists precisely so a crashed
instance doesn't wedge things for longer than that.

## Monitoring

The client CGI program answers a lightweight, unauthenticated health
check — no cookie or identity-provider check applies to `status` — good
for uptime monitoring:

```sh
curl --data '{"kind":"status"}' https://<your-site>/cgi-bin/stadhouder/client
```

A healthy response looks like:

```json
{"ok":true,"data":{"service":"stadhouder","version":"...","test_env":false,"time_ms":...,"service_last_run_ms":...,"service_memory_pct":null}}
```

Points worth watching:

- `test_env` should always read `false` in production — if it doesn't,
  `TEST_ENV=true` has been left on in `stadhouder.conf` (see *Security
  notes*).
- `service_last_run_ms` stalling (not advancing minute to minute)
  indicates cron isn't reaching the service program at all — check the
  crontab entry and the cron log/mail for errors, and confirm the service
  binary is still executable and in place.
- `service_memory_pct` climbing steadily toward `MEMORY_REJECT_PCT`
  (when `MEMORY_LIMIT_MB` is configured) is your early warning before
  `connect` starts refusing new connections - worth alerting on well
  before it gets there. `null` means either the limit isn't configured or
  no instance has run yet to measure it.
- A `status` call that times out or returns an HTML error page rather
  than JSON is almost always the CGI binary's execute permission (see
  *Installing the application*, step 4).

## Troubleshooting checklist

- **Client CGI calls fail immediately / return an error page instead of
  JSON** — check execute permissions (`0755`) on
  `cgi-bin/stadhouder/client`, and that neither it nor its parent
  directory is group/world-writable (suEXEC hosts refuse to run those).
- **`connect` is refused** — if `COOKIE_NAME` is set, confirm the
  browser is actually sending that cookie (e.g. the identity provider's
  session cookie); if `USER_ID_URL` is also set, confirm that endpoint is
  reachable *from the server* (it's called same-host, not from the
  browser) and returns the field named by `USER_ID_JSON`. If the error is
  specifically `"service memory limit exceeded"`, the service's measured
  RSS has reached `MEMORY_REJECT_PCT` of `MEMORY_LIMIT_MB` — see
  *Guarding against memory exhaustion* under *Configuration*; existing
  connections keep working, only new ones are turned away.
- **Connections silently stop working after ~2 minutes idle** — this is
  expected: a connection with no client message for two minutes is
  closed as `ServerIdleTimeout`. Well-behaved clients call `poll`
  frequently enough that heartbeats keep it alive; this is not a bug to
  chase unless clients are polling less often than that.
- **Service never seems to start** — check `stadhouder/state/last_run`
  is advancing (cron is firing) and that no stale `service_flag` is
  blocking a fresh instance from taking over (see *Runtime state*
  above).
- **Everything above looks fine but behavior is otherwise odd** (times
  drifting, things happening "early" or "late") — double-check
  `TEST_ENV` is `false`. A `true` value puts the instance on simulated
  time driven by `state/sim_time_diff` and `state/time_factor`, which is
  only meant for test harnesses.

## Security notes

- Stadhouder does no identity verification of its own beyond what
  `COOKIE_NAME`/`USER_ID_URL`/`USER_ID_JSON` are configured to do.
  `COOKIE_NAME` alone only checks a cookie is *present* — never its
  value — so it is not, by itself, an authentication check; pair it with
  `USER_ID_URL` pointed at a real identity provider for actual identity
  verification.
- `stadhouder/` (config, state, pipes, the service binary) sits outside
  `public_html/` specifically so none of it is ever directly
  downloadable over plain HTTP. Don't relocate it under the web root.
- Never enable `TEST_ENV` on a production instance. It switches the
  service onto simulated time controlled by writable files in
  `state/`, which is only safe on a harness nobody but you can reach.
- Because cron never waits for a previous tick's instance to finish
  before starting the next, and singleton coordination relies entirely
  on the `service_flag` file, don't run multiple applications' cron
  entries against the same `stadhouder/state/` directory — each
  application needs its own.
