mod casemap;
mod config;
mod connection;
mod db;
mod presence;
mod shutdown;

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::Config;
use crate::db::Db;
use crate::shutdown::Shutdown;

/// The settings file read when none is given. `.gitignore` excludes it.
const DEFAULT_CONFIG: &str = "lurker.toml";

const USAGE: &str = "usage: lurker [CONFIG]

Connects to IRC and keeps watch over the rooms in CONFIG, a TOML file
laid out like lurker.example.toml. CONFIG defaults to lurker.toml.";

/// What the command line asks for.
enum Args {
    Run(PathBuf),
    Help,
    Version,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let path = match parse_args() {
        Ok(Args::Run(path)) => path,
        Ok(Args::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Args::Version) => {
            println!("lurker {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("error: {err}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let db = match Db::open(&config.database.path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut shutdown = match Shutdown::new() {
        Ok(shutdown) => shutdown,
        Err(err) => {
            eprintln!("error: listening for stop signals: {err}");
            return ExitCode::FAILURE;
        }
    };

    connection::run(&config, &db, &mut shutdown).await;
    ExitCode::SUCCESS
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args_os().skip(1);

    let Some(first) = args.next() else {
        return Ok(Args::Run(PathBuf::from(DEFAULT_CONFIG)));
    };
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument '{}'", extra.to_string_lossy()));
    }

    match first.to_str() {
        Some("-h" | "--help") => Ok(Args::Help),
        Some("-V" | "--version") => Ok(Args::Version),
        Some(flag) if flag.starts_with('-') => Err(format!("unknown option '{flag}'")),
        _ => Ok(Args::Run(PathBuf::from(first))),
    }
}
