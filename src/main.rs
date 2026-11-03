mod admin;
mod casemap;
mod config;
mod connection;
mod dates;
mod db;
mod http;
mod messages;
mod presence;
mod rng;
mod schedule;
mod shutdown;
mod units;

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::Settings;
use crate::db::Db;
use crate::http::Status;
use crate::messages::Messages;
use crate::shutdown::Shutdown;

/// The settings file read when none is given. `.gitignore` excludes it.
const DEFAULT_CONFIG: &str = "lurker.toml";

const USAGE: &str = "usage: lurker [CONFIG]
       lurker --health ADDRESS

Connects to IRC and keeps watch over the rooms in CONFIG, a TOML file
laid out like lurker.example.toml. CONFIG defaults to lurker.toml.

--health asks the status page at ADDRESS, such as 127.0.0.1:8080, whether
lurker is connected to IRC, and exits with 0 when it is.";

/// What the command line asks for.
enum Args {
    Run(PathBuf),
    Health(String),
    Help,
    Version,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let path = match parse_args() {
        Ok(Args::Run(path)) => path,
        Ok(Args::Health(address)) => {
            return match http::check(&address) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::FAILURE
                }
            };
        }
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

    let mut settings = match Settings::load(&path) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let db = match Db::open(&settings.config.database.path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let messages = match Messages::builtin() {
        Ok(messages) => messages,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match &settings.config.http.listen {
        Some(address) => match http::bind(address).await {
            Ok(listener) => Some(listener),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let mut shutdown = match Shutdown::new() {
        Ok(shutdown) => shutdown,
        Err(err) => {
            eprintln!("error: listening for stop signals: {err}");
            return ExitCode::FAILURE;
        }
    };

    let status = Status::default();
    let page = async {
        match listener {
            Some(listener) => http::serve(listener, status.clone()).await,
            None => std::future::pending().await,
        }
    };

    // The status page runs until lurker stops
    tokio::select! {
        () = connection::run(&mut settings, &db, &messages, &status, &mut shutdown) => {}
        () = page => {}
    }
    ExitCode::SUCCESS
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args_os().skip(1);

    let Some(first) = args.next() else {
        return Ok(Args::Run(PathBuf::from(DEFAULT_CONFIG)));
    };

    if first == "--health" {
        let address = args
            .next()
            .ok_or("--health needs the status page's address, such as 127.0.0.1:8080")?;
        if let Some(extra) = args.next() {
            return Err(format!("unexpected argument '{}'", extra.to_string_lossy()));
        }
        return Ok(Args::Health(address.to_string_lossy().into_owned()));
    }

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
