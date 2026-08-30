use std::{env, io, path::PathBuf};

use jiandu_mcp::{MemoryExecutionContext, MemoryServer, serve_stdio};
use jiandu_memory::memory_store::MemoryStore;

struct Config {
    data_dir: PathBuf,
    session_id: String,
    project_id: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = parse_args(env::args().skip(1))?;
    let mut context = MemoryExecutionContext::new(config.session_id)?;
    if let Some(project_id) = config.project_id {
        context = context.with_project_id(project_id)?;
    }
    let server = MemoryServer::new(MemoryStore::new(config.data_dir), context);
    serve_stdio(server).await
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> io::Result<Config> {
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
                println!("Usage: jiandu --data-dir <PATH> --session-id <ID> [--project-id <ID>]");
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

    Ok(Config {
        data_dir: PathBuf::from(data_dir),
        session_id: session_id.ok_or_else(|| invalid_input("--session-id is required"))?,
        project_id,
    })
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_minimal_stdio_identity_flags() {
        let config = parse_args([
            "--data-dir".to_string(),
            "/tmp/jiandu-data".to_string(),
            "--session-id".to_string(),
            "session_1".to_string(),
            "--project-id".to_string(),
            "project_1".to_string(),
        ])
        .expect("valid args");
        assert_eq!(config.data_dir, PathBuf::from("/tmp/jiandu-data"));
        assert_eq!(config.session_id, "session_1");
        assert_eq!(config.project_id.as_deref(), Some("project_1"));
    }

    #[test]
    fn rejects_missing_and_unknown_flags() {
        assert!(parse_args(Vec::<String>::new()).is_err());
        assert!(parse_args(["--http".to_string()]).is_err());
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
