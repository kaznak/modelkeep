use std::{
    env,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::{self, Command},
    sync::Arc,
    time::Duration,
};

use modelkeep::{admin, http, pullthrough::PullThrough, upstream::OfficialHfFetcher, Archive};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let filter = log_filter();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run().await {
        tracing::error!(event = "process_failed", error = %error, "modelkeep command failed");
        process::exit(1);
    }
}

fn log_filter() -> EnvFilter {
    log_filter_from(env::var("RUST_LOG"))
}

fn log_filter_from(value: Result<String, env::VarError>) -> EnvFilter {
    match value {
        Ok(value) => EnvFilter::try_new(value).unwrap_or_else(|error| {
            eprintln!("modelkeep: invalid RUST_LOG filter; using info: {error}");
            EnvFilter::new("info")
        }),
        Err(env::VarError::NotPresent) => EnvFilter::new("info"),
        Err(error) => {
            eprintln!("modelkeep: invalid RUST_LOG value; using info: {error}");
            EnvFilter::new("info")
        }
    }
}

fn initialize_ownership(target: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        event = "ownership_initialization_started",
        target = %target.display(),
        owner = "10001:10001",
        "archive ownership initialization started"
    );
    let status = Command::new("/bin/chown")
        .arg("10001:10001")
        .arg(&target)
        .status()
        .map_err(|error| {
            tracing::error!(
                event = "ownership_initialization_failed",
                target = %target.display(),
                error = %error,
                "could not execute ownership initialization"
            );
            error
        })?;
    if !status.success() {
        tracing::error!(
            event = "ownership_initialization_failed",
            target = %target.display(),
            exit_status = %status,
            "archive ownership initialization failed"
        );
        return Err(format!("ownership initialization failed with {status}").into());
    }
    tracing::info!(
        event = "ownership_initialization_completed",
        target = %target.display(),
        owner = "10001:10001",
        "archive ownership initialization completed"
    );
    Ok(())
}

fn parse_remove_option(option: Option<&str>, has_extra: bool) -> Result<bool, String> {
    if has_extra {
        return Err("remove accepts only --dry-run".into());
    }
    match option {
        None => Ok(false),
        Some("--dry-run") => Ok(true),
        Some(_) => Err("remove accepts only --dry-run".into()),
    }
}

fn probe_endpoint(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let address = env::var("MODELKEEP_HEALTH_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:8090".to_string())
        .parse::<SocketAddr>()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request =
        format!("GET {endpoint} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut buffer = [0u8; 128];
    let read = stream.read(&mut buffer)?;
    if !String::from_utf8_lossy(&buffer[..read]).starts_with("HTTP/1.1 200") {
        return Err(format!("endpoint {endpoint} is not ready").into());
    }
    Ok(())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("list") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let repo = args.next().ok_or("list requires repository id")?;
            let archive = Archive::new(root)?;
            for commit in archive.list_revisions(&repo)? {
                println!("{commit}");
            }
            Ok(())
        }
        Some("show") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let repo = args.next().ok_or("show requires repository id")?;
            let commit = args.next().ok_or("show requires commit")?;
            let archive = Archive::new(root)?;
            print!("{}", archive.manifest(&repo, &commit)?);
            Ok(())
        }
        Some("import-hf-cache") => {
            let cache = args.next().ok_or("import-hf-cache requires cache path")?;
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let archive = Archive::new(root)?;
            archive.recover_incomplete()?;
            let report =
                modelkeep::importer::import_hf_cache(&archive, PathBuf::from(cache).as_path())?;
            println!(
                "imported {} revisions and {} refs",
                report.revisions, report.refs
            );
            Ok(())
        }
        Some("verify") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let repo = args.next().ok_or("verify requires repository id")?;
            let commit = args.next().ok_or("verify requires commit")?;
            let archive = Archive::new(root)?;
            let count = archive.verify_revision(&repo, &commit)?;
            println!("verified {count} files for {repo}@{commit}");
            Ok(())
        }
        Some("audit") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            if args.next().is_some() {
                return Err("audit accepts only archive-root".into());
            }
            let report = Archive::open_read_only(root)?.audit()?;
            println!(
                "{}",
                serde_json::json!({
                    "status": if report.failures.is_empty() { "clean" } else { "failed" },
                    "checked": report.checked,
                    "failures": report.failures.iter().map(|failure| serde_json::json!({
                        "repo_id": failure.repo_id, "commit": failure.commit, "error": failure.error
                    })).collect::<Vec<_>>()
                })
            );
            if report.failures.is_empty() {
                Ok(())
            } else {
                Err("archive audit failed".into())
            }
        }
        Some("remove") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let repo = args.next().ok_or("remove requires repository id")?;
            let commit = args.next().ok_or("remove requires commit")?;
            let dry_run = parse_remove_option(args.next().as_deref(), args.next().is_some())?;
            let archive = Archive::new(root)?;
            archive.recover_incomplete()?;
            let result = archive.remove_revision(&repo, &commit, dry_run)?;
            if result.removed {
                println!("removed {repo}@{commit}");
            } else {
                println!("would remove {repo}@{commit}");
            }
            Ok(())
        }
        Some("refresh") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let repo = args.next().ok_or("refresh requires repository id")?;
            let reference = args.next().ok_or("refresh requires mutable ref")?;
            let dry_run = parse_remove_option(args.next().as_deref(), args.next().is_some())?;
            let python = env::var("MODELKEEP_HF_PYTHON")?;
            let helper = env::var("MODELKEEP_HF_HELPER")?;
            let archive = Archive::new(root)?;
            archive.recover_incomplete()?;
            let pull = PullThrough::new(
                archive,
                Arc::new(OfficialHfFetcher {
                    python: python.into(),
                    helper: helper.into(),
                }),
            );
            let result = pull.refresh(&repo, &reference, dry_run)?;
            println!(
                "{} -> {}{}",
                result.previous.as_deref().unwrap_or("<none>"),
                result.proposed,
                if result.published { "" } else { " (dry-run)" }
            );
            Ok(())
        }
        Some("serve") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let bind_value = args.next().unwrap_or_else(|| "0.0.0.0:8090".to_string());
            let bind = bind_value.parse::<SocketAddr>().map_err(|error| {
                tracing::error!(
                    event = "configuration_failed",
                    field = "listen_address",
                    value = %bind_value,
                    error = %error,
                    "invalid listen address"
                );
                error
            })?;
            tracing::info!(
                event = "startup_started",
                version = env!("CARGO_PKG_VERSION"),
                archive_root = %root.display(),
                listen_address = %bind,
                "modelkeep startup started"
            );
            tracing::info!(
                event = "archive_initialization_started",
                archive_root = %root.display(),
                "archive initialization started"
            );
            let archive = Archive::new(root).map_err(|error| {
                tracing::error!(
                    event = "archive_initialization_failed",
                    error = %error,
                    "archive initialization failed"
                );
                error
            })?;
            tracing::info!(
                event = "archive_initialization_completed",
                "archive initialization completed"
            );
            tracing::info!(
                event = "archive_recovery_started",
                "archive recovery started"
            );
            let recovered = archive.recover_incomplete().map_err(|error| {
                tracing::error!(
                    event = "archive_recovery_failed",
                    error = %error,
                    "archive recovery failed"
                );
                error
            })?;
            tracing::info!(
                event = "archive_recovery_completed",
                recovered_staging_directories = recovered,
                "archive recovery completed"
            );
            archive.check_readiness().map_err(|error| {
                tracing::error!(
                    event = "archive_readiness_failed",
                    error = %error,
                    "archive did not become ready during startup"
                );
                error
            })?;
            let upstream = match (
                env::var("MODELKEEP_HF_PYTHON"),
                env::var("MODELKEEP_HF_HELPER"),
            ) {
                (Ok(python), Ok(helper)) => {
                    let fetcher = Arc::new(OfficialHfFetcher {
                        python: python.into(),
                        helper: helper.into(),
                    });
                    let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
                    Some(pullthrough)
                }
                _ => None,
            };
            let admin_config = admin::Config::from_env()?;
            tracing::info!(
                event = "startup_configuration",
                pullthrough_enabled = upstream.is_some(),
                management_enabled = admin_config.is_some(),
                "startup configuration loaded"
            );
            match (upstream, admin_config) {
                (Some(pullthrough), Some(config)) => {
                    tokio::try_join!(
                        http::serve_with_pullthrough(archive.clone(), pullthrough.clone(), bind),
                        admin::serve(archive, Some(pullthrough), config)
                    )?;
                }
                (None, Some(config)) => {
                    tokio::try_join!(
                        http::serve(archive.clone(), bind),
                        admin::serve(archive, None, config)
                    )?;
                }
                (Some(pullthrough), None) => {
                    http::serve_with_pullthrough(archive, pullthrough, bind).await?
                }
                (None, None) => http::serve(archive, bind).await?,
            }
            Ok(())
        }
        Some("init-ownership") => {
            let target = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            if args.next().is_some() {
                return Err("init-ownership accepts only target path".into());
            }
            initialize_ownership(target)
        }
        Some("health") => probe_endpoint("/healthz"),
        Some("ready") => probe_endpoint("/readyz"),
        Some("help") | None => {
            println!(
                "usage: modelkeep list [archive-root] <repo-id>
       modelkeep show [archive-root] <repo-id> <commit>
       modelkeep import-hf-cache <cache-path> [archive-root]
       modelkeep serve [archive-root] [bind-address]
       modelkeep init-ownership [target-path]
       modelkeep health
       modelkeep ready
       modelkeep audit [archive-root]
       modelkeep refresh [archive-root] <repo-id> <ref> [--dry-run]
       modelkeep verify [archive-root] <repo-id> <commit>
       modelkeep remove [archive-root] <repo-id> <commit> [--dry-run]"
            );
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{log_filter_from, parse_remove_option};
    use std::env::VarError;

    #[test]
    fn remove_option_parser_accepts_real_delete_and_dry_run() {
        assert!(!parse_remove_option(None, false).unwrap());
        assert!(parse_remove_option(Some("--dry-run"), false).unwrap());
    }

    #[test]
    fn remove_option_parser_rejects_unknown_and_extra_arguments() {
        assert!(parse_remove_option(Some("--other"), false).is_err());
        assert!(parse_remove_option(None, true).is_err());
        assert!(parse_remove_option(Some("--dry-run"), true).is_err());
    }

    #[test]
    fn log_filter_accepts_valid_directives_and_defaults_invalid_values() {
        assert_eq!(
            log_filter_from(Err(VarError::NotPresent)).to_string(),
            "info"
        );
        assert_eq!(
            log_filter_from(Ok("modelkeep=debug".into())).to_string(),
            "modelkeep=debug"
        );
        assert_eq!(log_filter_from(Ok("[".into())).to_string(), "info");
    }
}
