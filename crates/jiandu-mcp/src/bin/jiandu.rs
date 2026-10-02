use std::{env, io, path::PathBuf};

use jiandu_mcp::{MemoryExecutionContext, MemoryServer, serve_stdio, serve_ui};
use jiandu_memory::import_bamboo_durable_memory;
use jiandu_memory::memory_store::MemoryStore;

enum AppCommand {
    Serve(ServeConfig),
    ImportBamboo(ImportConfig),
    Ui(UiConfig),
}

struct ServeConfig {
    data_dir: PathBuf,
    session_id: Option<String>,
    project_id: Option<String>,
}

struct ImportConfig {
    source_data_dir: PathBuf,
    destination_data_dir: PathBuf,
}

struct UiConfig {
    data_dir: PathBuf,
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match parse_args(env::args().skip(1))? {
        AppCommand::Serve(config) => {
            let mut context = MemoryExecutionContext::default();
            if let Some(session_id) = config.session_id {
                context = context.with_session_id(session_id)?;
            }
            if let Some(project_id) = config.project_id {
                context = context.with_project_id(project_id)?;
            }
            let server = MemoryServer::new(MemoryStore::new(config.data_dir), context);
            serve_stdio(server).await
        }
        AppCommand::ImportBamboo(config) => {
            let report =
                import_bamboo_durable_memory(config.source_data_dir, config.destination_data_dir)
                    .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        AppCommand::Ui(config) => {
            serve_ui(config.data_dir, config.port).await?;
            Ok(())
        }
    }
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> io::Result<AppCommand> {
    let mut arguments = arguments.into_iter();
    match arguments.next() {
        Some(command) if command == "ui" => parse_ui_args(arguments).map(AppCommand::Ui),
        Some(command) if command == "import-bamboo" => {
            parse_import_args(arguments).map(AppCommand::ImportBamboo)
        }
        Some(first) => {
            parse_serve_args(std::iter::once(first).chain(arguments)).map(AppCommand::Serve)
        }
        None => parse_serve_args(std::iter::empty()).map(AppCommand::Serve),
    }
}

fn parse_ui_args(arguments: impl IntoIterator<Item = String>) -> io::Result<UiConfig> {
    let mut data_dir = None;
    let mut port = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let target = match argument.as_str() {
            "--data-dir" => &mut data_dir,
            "--port" => &mut port,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => return Err(invalid_input(format!("unknown ui argument: {argument}"))),
        };
        if target.is_some() {
            return Err(invalid_input(format!("duplicate argument: {argument}")));
        }
        *target = Some(
            arguments
                .next()
                .ok_or_else(|| invalid_input(format!("missing value for {argument}")))?,
        );
    }
    let data_dir = match data_dir {
        Some(path) => required_path(Some(path), "--data-dir")?,
        None => {
            let user_dir = env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
                .ok_or_else(|| {
                    invalid_input("user directory is unavailable; provide --data-dir")
                })?;
            PathBuf::from(user_dir).join(".jiandu")
        }
    };
    let port = port
        .as_deref()
        .unwrap_or("0")
        .parse::<u16>()
        .map_err(|_| invalid_input("--port must be an integer between 0 and 65535"))?;
    Ok(UiConfig { data_dir, port })
}

fn parse_serve_args(arguments: impl IntoIterator<Item = String>) -> io::Result<ServeConfig> {
    let mut data_dir = None;
    let mut session_id = None;
    let mut project_id = None;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        let target = match argument.as_str() {
            "--data-dir" => &mut data_dir,
            "--session-id" => &mut session_id,
            "--project-id" => &mut project_id,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(invalid_input(format!("unknown argument: {other}"))),
        };
        if target.is_some() {
            return Err(invalid_input(format!("duplicate argument: {argument}")));
        }
        *target = Some(
            arguments
                .next()
                .ok_or_else(|| invalid_input(format!("missing value for {argument}")))?,
        );
    }

    let data_dir = data_dir.ok_or_else(|| invalid_input("--data-dir is required"))?;
    if data_dir.trim().is_empty() {
        return Err(invalid_input("--data-dir cannot be empty"));
    }

    Ok(ServeConfig {
        data_dir: PathBuf::from(data_dir),
        session_id,
        project_id,
    })
}

fn parse_import_args(arguments: impl IntoIterator<Item = String>) -> io::Result<ImportConfig> {
    let mut source_data_dir = None;
    let mut destination_data_dir = None;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        let target = match argument.as_str() {
            "--source-data-dir" => &mut source_data_dir,
            "--data-dir" => &mut destination_data_dir,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(invalid_input(format!("unknown import argument: {other}"))),
        };
        if target.is_some() {
            return Err(invalid_input(format!("duplicate argument: {argument}")));
        }
        *target = Some(
            arguments
                .next()
                .ok_or_else(|| invalid_input(format!("missing value for {argument}")))?,
        );
    }

    let source_data_dir = required_path(source_data_dir, "--source-data-dir")?;
    let destination_data_dir = required_path(destination_data_dir, "--data-dir")?;
    Ok(ImportConfig {
        source_data_dir,
        destination_data_dir,
    })
}

fn required_path(value: Option<String>, flag: &str) -> io::Result<PathBuf> {
    let value = value.ok_or_else(|| invalid_input(format!("{flag} is required")))?;
    if value.trim().is_empty() {
        return Err(invalid_input(format!("{flag} cannot be empty")));
    }
    Ok(PathBuf::from(value))
}

fn print_help() {
    println!(
        "Usage:\n  jiandu --data-dir <PATH> [--session-id <ID>] [--project-id <ID>]\n  jiandu ui [--data-dir <PATH>] [--port <PORT>]\n  jiandu import-bamboo --source-data-dir <BAMBOO_DATA_DIR> --data-dir <EMPTY_JIANDU_DATA_DIR>\n\nSession and Project ids are optional defaults. Hosts can supply per-call identities in tools/call _meta[\"io.github.bigduu.jiandu/context\"].\nThe console defaults to ~/.jiandu and an available IPv4 loopback port."
    );
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_needs_no_session_or_project_identity() {
        let AppCommand::Ui(defaults) = parse_args(["ui".into()]).unwrap() else {
            panic!("expected console");
        };
        assert!(defaults.data_dir.ends_with(".jiandu"));
        assert_eq!(defaults.port, 0);
        let AppCommand::Ui(config) = parse_args([
            "ui".into(),
            "--data-dir".into(),
            "/tmp/console".into(),
            "--port".into(),
            "9123".into(),
        ])
        .unwrap() else {
            panic!("expected console");
        };
        assert_eq!(config.data_dir, PathBuf::from("/tmp/console"));
        assert_eq!(config.port, 9123);
    }

    #[test]
    fn console_rejects_identity_host_and_invalid_port_flags() {
        for args in [
            vec!["ui", "--session-id", "session"],
            vec!["ui", "--project-id", "project"],
            vec!["ui", "--host", "0.0.0.0"],
            vec!["ui", "--data-dir", " "],
            vec!["ui", "--port", "65536"],
            vec!["ui", "--port", "-1"],
            vec!["ui", "--port"],
            vec!["ui", "--port", "0", "--port", "1"],
        ] {
            assert!(parse_args(args.into_iter().map(str::to_string)).is_err());
        }
    }

    #[test]
    fn parses_optional_stdio_identity_defaults() {
        let command = parse_args([
            "--data-dir".to_string(),
            "/tmp/jiandu-data".to_string(),
            "--session-id".to_string(),
            "session_1".to_string(),
            "--project-id".to_string(),
            "project_1".to_string(),
        ])
        .expect("valid args");
        let AppCommand::Serve(config) = command else {
            panic!("expected serve command")
        };
        assert_eq!(config.data_dir, PathBuf::from("/tmp/jiandu-data"));
        assert_eq!(config.session_id.as_deref(), Some("session_1"));
        assert_eq!(config.project_id.as_deref(), Some("project_1"));
    }

    #[test]
    fn starts_without_session_or_project_defaults() {
        let command = parse_args(["--data-dir".to_string(), "/tmp/jiandu-data".to_string()])
            .expect("data-dir is the only required serve flag");
        let AppCommand::Serve(config) = command else {
            panic!("expected serve command")
        };
        assert!(config.session_id.is_none());
        assert!(config.project_id.is_none());
    }

    #[test]
    fn project_default_does_not_require_a_session_default() {
        let command = parse_args([
            "--data-dir".to_string(),
            "/tmp/jiandu-data".to_string(),
            "--project-id".to_string(),
            "project_1".to_string(),
        ])
        .expect("Project-only default");
        let AppCommand::Serve(config) = command else {
            panic!("expected serve command")
        };
        assert!(config.session_id.is_none());
        assert_eq!(config.project_id.as_deref(), Some("project_1"));
    }

    #[test]
    fn parses_the_one_shot_bamboo_import_flags() {
        let command = parse_args([
            "import-bamboo".to_string(),
            "--source-data-dir".to_string(),
            "/tmp/bamboo-data".to_string(),
            "--data-dir".to_string(),
            "/tmp/jiandu-data".to_string(),
        ])
        .expect("valid import args");
        let AppCommand::ImportBamboo(config) = command else {
            panic!("expected import command")
        };
        assert_eq!(config.source_data_dir, PathBuf::from("/tmp/bamboo-data"));
        assert_eq!(
            config.destination_data_dir,
            PathBuf::from("/tmp/jiandu-data")
        );
    }

    #[test]
    fn rejects_missing_and_unknown_flags() {
        assert!(parse_args(Vec::<String>::new()).is_err());
        assert!(parse_args(["--http".to_string()]).is_err());
        assert!(
            parse_args([
                "import-bamboo".to_string(),
                "--source-data-dir".to_string(),
                "/tmp/bamboo-data".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "import-bamboo".to_string(),
                "--source-data-dir".to_string(),
                "/tmp/bamboo-data".to_string(),
                "--data-dir".to_string(),
                " ".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "--data-dir".to_string(),
                "  ".to_string(),
                "--session-id".to_string(),
                "session_1".to_string(),
            ])
            .is_err()
        );
    }
}
