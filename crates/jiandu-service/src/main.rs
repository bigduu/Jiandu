use jiandu_service::{RunningDaemon, ServeConfig};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

const USAGE: &str = "usage: jiandu serve --config <local-config.json>";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("jiandu: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = parse_args(std::env::args_os())?;
    let config = ServeConfig::load(config_path)?;
    let mut daemon = RunningDaemon::start(config).await?;
    tokio::select! {
        biased;
        result = daemon.wait() => result?,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            daemon.shutdown().await?;
        }
    }
    Ok(())
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<PathBuf, CliError> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let command = arguments.next();
    let flag = arguments.next();
    let value = arguments.next();
    let value_looks_inline = value.as_deref().is_some_and(|value| {
        value.is_empty()
            || value.to_str().is_some_and(|value| {
                let value = value.trim_start();
                value.starts_with('{') || value.starts_with('[')
            })
    });
    if command.as_deref() != Some(std::ffi::OsStr::new("serve"))
        || flag.as_deref() != Some(std::ffi::OsStr::new("--config"))
        || value.as_deref() == Some(std::ffi::OsStr::new("-"))
        || value_looks_inline
        || arguments.next().is_some()
    {
        return Err(CliError);
    }
    value.map(PathBuf::from).ok_or(CliError)
}

#[derive(Debug)]
struct CliError;

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(USAGE)
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_only_serve_with_one_local_config_path() {
        assert_eq!(
            parse_args(["jiandu", "serve", "--config", "daemon.json"].map(OsString::from))
                .expect("accepted command"),
            PathBuf::from("daemon.json")
        );
        for arguments in [
            vec!["jiandu"],
            vec!["jiandu", "serve"],
            vec!["jiandu", "serve", "--config", "-"],
            vec!["jiandu", "serve", "--config", "{}"],
            vec!["jiandu", "serve", "--token", "RAW_TOKEN_SENTINEL"],
            vec!["jiandu", "serve", "--config", "daemon.json", "extra"],
            vec!["jiandu", "status", "--config", "daemon.json"],
        ] {
            assert!(
                parse_args(arguments.into_iter().map(OsString::from)).is_err(),
                "unexpected CLI shape accepted"
            );
        }
    }
}
