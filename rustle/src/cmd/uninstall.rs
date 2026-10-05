//! `rustle uninstall`: remove the service and the binary.

use super::service::{self, Action};

#[derive(clap::Args)]
pub struct Args {
    /// Also remove ~/.config/rustle (the config and its API key)
    #[arg(long)]
    pub purge: bool,
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let purge = args.purge;
    // The service first (user scope, then system scope when it is ours).
    for system in [false, true] {
        let cmd = service::Args {
            action: Action::Uninstall,
            system,
            config: None,
            dry_run: false,
        };
        if let Err(e) = service::run(cmd) {
            if !system {
                eprintln!("note: user service: {e:#}");
            }
        }
        if cfg!(target_os = "macos") {
            break;
        }
    }
    if purge {
        if let Ok(dir) = super::user_config_dir() {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
                println!("removed {}", dir.display());
            }
        }
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    std::fs::remove_file(&exe)?;
    println!("removed {}", exe.display());
    Ok(0)
}
