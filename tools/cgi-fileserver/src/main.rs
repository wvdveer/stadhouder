use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::Response,
    routing::any,
    Router,
};
use clap::Parser;
use std::{
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::io::AsyncWriteExt;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// A simple HTTP static file server that also runs CGI programs found under a `cgi-bin` folder
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Directory to serve files from
    #[arg(short, long, default_value = "./files")]
    dir: PathBuf,

    /// Address to bind to
    #[arg(short, long, default_value = "127.0.0.1")]
    addr: String,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    port: u16,
}

struct AppState {
    root: PathBuf,
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fileserver=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();

    if !args.dir.exists() {
        tracing::warn!(
            "Serve directory {:?} does not exist — creating it.",
            args.dir
        );
        std::fs::create_dir_all(&args.dir)?;
    }

    let root = args.dir.canonicalize()?;
    tracing::info!("Serving files from {:?}", root);

    let cgi_bin = root.join("cgi-bin");
    if cgi_bin.is_dir() {
        tracing::info!("Found cgi-bin folder — {:?} will be treated as CGI programs, not static files", cgi_bin);
    } else {
        tracing::info!("No cgi-bin folder found under the served directory — /cgi-bin/* will 404 until one exists");
    }

    let state = Arc::new(AppState {
        root: root.clone(),
        port: args.port,
    });

    let serve_dir = ServeDir::new(&root).append_index_html_on_directories(true);

    let app = Router::new()
        .route("/cgi-bin/*rest", any(cgi_handler))
        .fallback_service(serve_dir)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.addr, args.port).parse()?;
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Extensions tried, in order, when the exact requested name has no matching file.
/// Lets a GoDaddy-style extensionless URL (`/cgi-bin/envtest`) find a locally-built
/// `envtest.exe` etc., so local test URLs match production URLs exactly.
const CGI_EXTENSIONS: &[&str] = &["", ".exe", ".cmd", ".bat", ".com"];

/// Resolve the CGI script on disk: walks the requested path from longest to shortest
/// prefix (matching real Apache mod_cgi behaviour), so `/cgi-bin/sub/script/extra`
/// finds an executable at `cgi-bin/sub/script` and treats `/extra` as PATH_INFO.
/// Returns (actual file to execute, script name as requested — extensionless, PATH_INFO).
fn resolve_cgi_script(cgi_bin: &FsPath, rest: &str) -> Option<(PathBuf, String, String)> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }

    for split in (1..=segments.len()).rev() {
        let candidate_rel = segments[..split].join("/");

        for ext in CGI_EXTENSIONS {
            let candidate = cgi_bin.join(format!("{candidate_rel}{ext}"));

            let Ok(canonical) = candidate.canonicalize() else {
                continue;
            };
            // Prevent path traversal escaping cgi-bin via ".." segments.
            if !canonical.starts_with(cgi_bin) {
                continue;
            }
            if canonical.is_file() {
                let path_info = if split < segments.len() {
                    format!("/{}", segments[split..].join("/"))
                } else {
                    String::new()
                };
                return Some((canonical, candidate_rel, path_info));
            }
        }
    }

    None
}

fn header_to_env_name(name: &str) -> String {
    let mut env_name = String::from("HTTP_");
    for c in name.chars() {
        if c == '-' {
            env_name.push('_');
        } else {
            env_name.extend(c.to_uppercase());
        }
    }
    env_name
}

async fn cgi_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    Path(rest): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let cgi_bin = state.root.join("cgi-bin");

    let Some((script_path, script_name_rel, path_info)) = resolve_cgi_script(&cgi_bin, &rest)
    else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found (no matching CGI script)"))
            .unwrap();
    };

    let mut cmd = tokio::process::Command::new(&script_path);
    if let Some(parent) = script_path.parent() {
        cmd.current_dir(parent);
    }

    cmd.env("GATEWAY_INTERFACE", "CGI/1.1");
    cmd.env("SERVER_PROTOCOL", "HTTP/1.1");
    cmd.env("SERVER_SOFTWARE", "fileserver-cgi-dev/0.1");
    cmd.env("SERVER_NAME", "localhost");
    cmd.env("SERVER_PORT", state.port.to_string());
    cmd.env("REQUEST_METHOD", method.as_str());
    cmd.env("SCRIPT_NAME", format!("/cgi-bin/{script_name_rel}"));
    cmd.env("PATH_INFO", &path_info);
    cmd.env("REQUEST_URI", uri.to_string());
    cmd.env("QUERY_STRING", uri.query().unwrap_or(""));
    cmd.env("REMOTE_ADDR", remote_addr.ip().to_string());
    cmd.env("DOCUMENT_ROOT", state.root.to_string_lossy().to_string());
    cmd.env("CONTENT_LENGTH", body.len().to_string());
    cmd.env(
        "CONTENT_TYPE",
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    );

    for (name, value) in headers.iter() {
        if name == axum::http::header::CONTENT_TYPE || name == axum::http::header::CONTENT_LENGTH {
            continue;
        }
        if let Ok(value_str) = value.to_str() {
            cmd.env(header_to_env_name(name.as_str()), value_str);
        }
    }

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!(
                    "failed to run CGI script {:?}: {e}",
                    script_path
                )))
                .unwrap();
        }
    };

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let write_handle = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });

    let output = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("CGI script execution failed: {e}")))
                .unwrap();
        }
    };
    let _ = write_handle.await;

    if !output.stderr.is_empty() {
        tracing::warn!(
            "CGI script {:?} stderr:\n{}",
            script_path,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    build_response_from_cgi_output(&output.stdout)
}

/// STRICT, like Apache's mod_cgi: malformed CGI output is answered with a
/// 500, never patched up. This parser used to skip junk lines leniently,
/// which masked a real production bug - a library `println!` landing in the
/// CGI stdout ahead of the headers worked in every local test here while
/// the real Apache host answered "malformed header from script" (500).
fn malformed_cgi_response(reason: &str, raw: &[u8]) -> Response {
    tracing::error!(
        "malformed CGI output ({reason}); raw output starts: {:?}",
        String::from_utf8_lossy(&raw[..raw.len().min(200)])
    );
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(axum::http::header::CONTENT_TYPE, "text/plain")
        .body(Body::from(format!(
            "500 malformed CGI output: {reason}\n(strict parsing, matching Apache mod_cgi - \
             a real host would answer its generic 500 page here)\n"
        )))
        .unwrap()
}

fn build_response_from_cgi_output(raw: &[u8]) -> Response {
    let split_at = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));

    let Some((idx, seplen)) = split_at else {
        return malformed_cgi_response("no header/body separator in the output", raw);
    };
    let (header_bytes, body_bytes): (&[u8], &[u8]) = (&raw[..idx], &raw[idx + seplen..]);

    let header_text = String::from_utf8_lossy(header_bytes);
    let mut status_code = StatusCode::OK;
    let mut builder = Response::builder();

    for line in header_text.split(['\n']) {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return malformed_cgi_response(&format!("non-header line in the header block: {line:?}"), raw);
        };
        let name = name.trim();
        let value = value.trim();

        if name.eq_ignore_ascii_case("status") {
            if let Some(code_str) = value.split_whitespace().next() {
                if let Ok(code) = code_str.parse::<u16>() {
                    status_code = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
                }
            }
            continue;
        }

        builder = builder.header(name, value);
    }

    builder
        .status(status_code)
        .body(Body::from(body_bytes.to_vec()))
        .unwrap()
}
