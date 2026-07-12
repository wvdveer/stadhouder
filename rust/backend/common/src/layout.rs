use std::env;
use std::path::{Path, PathBuf};

/// `<DOCUMENT_ROOT's parent>` - the account home on cPanel, `site/`
/// locally. Resolved from DOCUMENT_ROOT (which survives suEXEC on cPanel,
/// unlike `SetEnv`-based config); the cron-started service program gets it
/// from its cron line rather than a web server.
pub fn home_dir() -> Result<PathBuf, String> {
    let doc_root =
        env::var("DOCUMENT_ROOT").map_err(|_| "DOCUMENT_ROOT env var not set".to_string())?;

    Path::new(&doc_root)
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("could not determine parent of DOCUMENT_ROOT '{doc_root}'"))
}

/// `<home>/stadhouder/` - everything stadhouder keeps on disk (cfg/,
/// state/, bin/, and on Linux pipes/) lives under this one folder, outside
/// the web root: anything under the web root is directly downloadable over
/// plain HTTP. One folder, not bare top-level `cfg/`, `state/`...: on a
/// shared cPanel account a generic top-level name is exactly the kind
/// likely to collide with something the main site creates.
pub fn stadhouder_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join("stadhouder"))
}

/// A connection uuid with underscores replacing dashes - the form used in
/// every filename derived from a connection (profile files, pipe names), so
/// the names stay conflict-free and filesystem/pipe-namespace friendly.
pub fn underscore_uuid(connection_id: &str) -> String {
    connection_id.replace('-', "_")
}
