//! Things derived from the current CGI request's own environment (as
//! opposed to `layout`, which is about the filesystem).
use std::env;

/// The absolute URL for `path` on this same request's host and port - used
/// to call another CGI program on the same site (see
/// `Config::user_id_url`). Built from the standard CGI environment
/// variables (`SERVER_NAME`, `SERVER_PORT`, `HTTPS`), which every
/// CGI-capable web server (and `cgi-fileserver`) sets for every request.
pub fn same_host_url(path: &str) -> Result<String, String> {
    let server_name =
        env::var("SERVER_NAME").map_err(|_| "SERVER_NAME env var not set".to_string())?;
    let server_port =
        env::var("SERVER_PORT").map_err(|_| "SERVER_PORT env var not set".to_string())?;
    let https = env::var("HTTPS").map(|v| v == "on").unwrap_or(false);

    Ok(build_url(&server_name, &server_port, https, path))
}

fn build_url(server_name: &str, server_port: &str, https: bool, path: &str) -> String {
    let scheme = if https { "https" } else { "http" };
    format!("{scheme}://{server_name}:{server_port}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_http_url_by_default() {
        assert_eq!(
            build_url("localhost", "8080", false, "/cgi-bin/keyscarf/api_session_check"),
            "http://localhost:8080/cgi-bin/keyscarf/api_session_check"
        );
    }

    #[test]
    fn builds_https_url_when_flagged() {
        assert_eq!(
            build_url("example.com", "443", true, "/cgi-bin/x"),
            "https://example.com:443/cgi-bin/x"
        );
    }
}
