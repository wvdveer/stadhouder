//! The test harness's stand-in for cron. A real deployment's cron line
//! fires the application's service executable every REAL minute:
//!   * * * * * DOCUMENT_ROOT=$HOME/public_html $HOME/stadhouder/bin/<app>
//! cron-sim fires it every SIMULATED minute instead - 60 seconds of engine
//! time, scaled by TIME_FACTOR like every other wait in this project - so
//! a test can compress hours of cron-driven service lifecycle into a few
//! real seconds by cranking TIME_FACTOR up.
//!
//! Like real cron, each tick launches a fresh instance and never waits for
//! it - most ticks launch into a live singleton (see the `stadhouder`
//! library's flag-file check) and the new instance exits almost
//! immediately, leaving the already-running one alone. Started by
//! start-site.sh, pointed at the harness's test-app; runs until killed.
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use clap::Parser;
use common::{time, Config};

/// One simulated minute, in engine milliseconds - matches the real cron
/// line's one-real-minute cadence.
const ONE_SIMULATED_MINUTE_MS: u64 = 60_000;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// The service executable to launch each tick.
    #[arg(long)]
    exe: PathBuf,

    /// DOCUMENT_ROOT to set for each launched instance - what a real cron
    /// line's `DOCUMENT_ROOT=...` prefix would supply.
    #[arg(long)]
    document_root: PathBuf,
}

fn main() {
    let args = Args::parse();

    // Also this process's own DOCUMENT_ROOT: its tick interval reads
    // TIME_FACTOR the same way the service and client do, via the same
    // stadhouder/state/ that --document-root's parent resolves to.
    std::env::set_var("DOCUMENT_ROOT", &args.document_root);

    eprintln!(
        "cron-sim: firing {} every simulated minute (DOCUMENT_ROOT={})",
        args.exe.display(),
        args.document_root.display()
    );

    // Fired-and-forgotten instances from previous ticks, reaped
    // opportunistically each loop so the list doesn't grow forever - most
    // exit almost immediately after deferring to a live singleton.
    let mut children: Vec<Child> = Vec::new();

    // Unlike real cron (which only ever fires at a minute boundary after
    // being installed), the first tick fires immediately: a developer
    // starting the site wants a live instance right away, not after
    // waiting out a full simulated minute.
    fire(&args, &mut children);

    loop {
        std::thread::sleep(Duration::from_millis(next_tick_ms()));
        fire(&args, &mut children);
    }
}

/// The real time to sleep for one simulated minute: TIME_FACTOR-scaled,
/// like every other wait in this project. Falls back to a real minute if
/// the config/state can't be read (e.g. not staged yet) - matching cron's
/// bare one-real-minute cadence rather than spinning.
fn next_tick_ms() -> u64 {
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("cron-sim: failed to load config: {e}");
            return ONE_SIMULATED_MINUTE_MS;
        }
    };
    match time::wait_factor(&config) {
        Ok(factor) => time::scale_wait_ms(ONE_SIMULATED_MINUTE_MS, factor),
        Err(e) => {
            eprintln!("cron-sim: failed to read TIME_FACTOR: {e}");
            ONE_SIMULATED_MINUTE_MS
        }
    }
}

fn fire(args: &Args, children: &mut Vec<Child>) {
    children.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) => true,
        Err(e) => {
            eprintln!("cron-sim: failed to check a previously launched instance: {e}");
            true
        }
    });

    match Command::new(&args.exe)
        .env("DOCUMENT_ROOT", &args.document_root)
        .spawn()
    {
        Ok(child) => {
            eprintln!("cron-sim: launched {} (pid {})", args.exe.display(), child.id());
            children.push(child);
        }
        // Not staged yet, or some other launch failure - log and try
        // again next tick, same as a misconfigured real cron job would.
        Err(e) => eprintln!("cron-sim: failed to launch {}: {e}", args.exe.display()),
    }
}
