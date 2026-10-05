//! `rustle uninstall`: remove the service and the binary.

use super::service;
use crate::cli::ServiceAction;

pub fn run(purge: bool) -> anyhow::Result<i32> {
    // The service first (user scope, then system scope when it is ours).
    for system in [false, true] {
        let cmd = crate::cli::ServiceCmd {
            action: ServiceAction::Uninstall,
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
