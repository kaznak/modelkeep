use std::{
    env,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process,
    sync::Arc,
    time::Duration,
};

use modelkeep::{http, pullthrough::PullThrough, upstream::OfficialHfFetcher, Archive};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    if let Err(error) = run().await {
        eprintln!("modelkeep: {error}");
        process::exit(1);
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
        Some("remove") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let repo = args.next().ok_or("remove requires repository id")?;
            let commit = args.next().ok_or("remove requires commit")?;
            let dry_run = match args.next().as_deref() {
                None => false,
                Some("--dry-run") => {
                    if args.next().is_some() {
                        return Err("remove accepts only --dry-run".into());
                    }
                    true
                }
                Some(_) => return Err("remove accepts only --dry-run".into()),
            };
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
        Some("serve") => {
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let bind = args
                .next()
                .unwrap_or_else(|| "0.0.0.0:8090".to_string())
                .parse::<SocketAddr>()?;
            let archive = Archive::new(root)?;
            archive.recover_incomplete()?;
            match (
                env::var("MODELKEEP_HF_PYTHON"),
                env::var("MODELKEEP_HF_HELPER"),
            ) {
                (Ok(python), Ok(helper)) => {
                    let fetcher = Arc::new(OfficialHfFetcher {
                        python: python.into(),
                        helper: helper.into(),
                    });
                    let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
                    http::serve_with_pullthrough(archive, pullthrough, bind).await?;
                }
                _ => http::serve(archive, bind).await?,
            }
            Ok(())
        }
        Some("health") => probe_endpoint("/healthz"),
        Some("ready") => probe_endpoint("/readyz"),
        Some("help") | None => {
            println!(
                "usage: modelkeep list [archive-root] <repo-id>
       modelkeep show [archive-root] <repo-id> <commit>
       modelkeep import-hf-cache <cache-path> [archive-root]
       modelkeep serve [archive-root] [bind-address]
       modelkeep health
       modelkeep ready
       modelkeep verify [archive-root] <repo-id> <commit>
       modelkeep remove [archive-root] <repo-id> <commit> [--dry-run]"
            );
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}").into()),
    }
}
