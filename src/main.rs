use std::{env, net::SocketAddr, path::PathBuf, process, sync::Arc};

use modelkeep::{http, pullthrough::PullThrough, upstream::OfficialHfFetcher, Archive};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("modelkeep: {error}");
        process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("import-hf-cache") => {
            let cache = args.next().ok_or("import-hf-cache requires cache path")?;
            let root = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data"));
            let archive = Archive::new(root)?;
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
        Some("help") | None => {
            println!(
                "usage: modelkeep import-hf-cache <cache-path> [archive-root]
       modelkeep serve [archive-root] [bind-address]
       modelkeep verify [archive-root] <repo-id> <commit>"
            );
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}").into()),
    }
}
