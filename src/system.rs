//! `system prune`: delete everything nothing needs anymore.

use crate::{image, network, paths, run, state};
use anyhow::{Result, bail};
use std::collections::HashSet;
use std::io::IsTerminal;

pub fn prune(all: bool, force: bool) -> Result<()> {
    if !force && !confirm(all)? {
        return Ok(());
    }
    let before = disk_usage(&paths::root());

    for s in state::all() {
        // Taking the lock also keeps a container that is being created or
        // started from being deleted under it.
        if s.is_running() {
            continue;
        }
        let Ok(_lock) = state::lock(&s.id, std::time::Duration::ZERO) else { continue };
        let short: String = s.id.chars().take(state::SHORT_ID).collect();
        let label = match &s.name {
            Some(name) => format!("{short} ({name})"),
            None => short,
        };
        match run::cleanup(&s) {
            Ok(()) => println!("deleted container {label}"),
            Err(e) => eprintln!("warning: delete container {label}: {e:#}"),
        }
    }

    let remaining = state::all();
    // Endpoints and namespaces left by killed supervisors.
    network::sweep();
    let used: HashSet<_> = remaining.iter().filter_map(|s| s.network.as_deref()).collect();
    match network::managed() {
        Ok(names) => {
            for name in names.iter().filter(|n| !used.contains(n.as_str())) {
                match network::delete(name) {
                    Ok(()) => println!("deleted network {name}"),
                    Err(e) => eprintln!("warning: delete network {name}: {e:#}"),
                }
            }
        }
        Err(e) => eprintln!("warning: list networks: {e:#}"),
    }

    if all {
        let used: HashSet<_> = remaining.iter().map(|s| s.image.as_str()).collect();
        for record in image::all().iter().filter(|r| !used.contains(r.reference.as_str())) {
            match std::fs::remove_file(paths::image(&record.reference)) {
                Ok(()) => println!("deleted image {}", record.reference),
                Err(e) => eprintln!("warning: delete image {}: {e}", record.reference),
            }
        }
    }
    image::collect_garbage();

    let after = disk_usage(&paths::root());
    println!("reclaimed {}", human(before.saturating_sub(after)));
    Ok(())
}

fn confirm(all: bool) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("system prune deletes data; pass --force to run it without a terminal");
    }
    let images = if all { ", images no container uses" } else { "" };
    eprint!("This deletes all containers that are not running, networks they no longer need{images}, and unused layers.\nContinue? [y/N] ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Bytes of the files under `dir`, without following reparse points.
fn disk_usage(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            total += disk_usage(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn human() {
        assert_eq!(super::human(512), "512 B");
        assert_eq!(super::human(1536), "1.5 KB");
        assert_eq!(super::human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
