# cgi-fileserver

A local dev tool: a minimal HTTP static file server that also runs CGI programs.
If the served directory contains a `cgi-bin` subfolder, requests under `/cgi-bin/`
are executed as CGI programs (per the classic CGI/1.1 protocol) instead of being
served as static files — mimicking real cPanel/Apache hosting so our `cgi`-crate
binaries can be exercised locally without deploying anywhere.

Built with [Axum](https://github.com/tokio-rs/axum) + [tower-http](https://github.com/tower-rs/tower-http).

## Features

- Serves any directory of files over plain HTTP
- Directory listing with `index.html` fallback
- Requests under `/cgi-bin/` run the matching executable there, feeding it real CGI
  environment variables (`REQUEST_METHOD`, `QUERY_STRING`, `CONTENT_LENGTH`,
  `HTTP_*` headers, `REMOTE_ADDR`, `DOCUMENT_ROOT`, etc.), the request body on stdin,
  and turns its stdout (a `Status:` line + headers + body) into the HTTP response.
- Nested paths under `cgi-bin` are supported (e.g. `/cgi-bin/sub/script/extra` finds
  an executable at `cgi-bin/sub/script` and passes `/extra` as `PATH_INFO`).
- The CGI script's stderr is logged to this server's own console, which real cPanel
  hosting often doesn't surface at all — handy for debugging.

## Building

```bash
cargo build --release
# Binary is at: target/release/fileserver
```

## Running

```bash
./target/release/fileserver --dir ./files --addr 127.0.0.1 --port 8080
```

Put a compiled CGI binary (a native Windows build of one of the project's
`rust/backend` bins works directly — no cross-compiling needed for local testing) in
`./files/cgi-bin/` and hit `http://127.0.0.1:8080/cgi-bin/<name>`.

### All flags

| Flag     | Default     | Description         |
|----------|-------------|----------------------|
| `--dir`  | `./files`   | Directory to serve   |
| `--addr` | `127.0.0.1` | Bind address         |
| `--port` | `8080`      | Port to listen on    |

Verbose logging: `RUST_LOG=debug ./target/release/fileserver`

## Security notes

- This is a **development tool**, not hardened for production/public exposure — it
  will run any executable placed under `cgi-bin/` with no sandboxing.
- Path traversal is prevented for both the static file side (`tower-http`'s `ServeDir`)
  and the CGI script resolution (canonicalized and checked against the `cgi-bin` root).
