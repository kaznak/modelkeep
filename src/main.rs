use std::{env, net::SocketAddr, path::PathBuf, process};

use modelkeep::{http, Archive};

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
            http::serve(archive, bind).await?;
            Ok(())
        }
        Some("help") | None => {
            println!(
                "usage: modelkeep serve [archive-root] [bind-address]
       modelkeep verify [archive-root] <repo-id> <commit>"
            );
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}").into()),
    }
}
