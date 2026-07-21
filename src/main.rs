use std::{env, path::Path};

use moss::storage;
#[cfg(any(target_os = "linux", target_os = "android"))]
use moss::fuse_driver;
#[cfg(target_os = "windows")]
use moss::windows_driver;
use moss::tui;

fn print_usage(program: &str) {
    eprintln!(
        "Usage:\n\
         \x20 {program} attach <storage.moss> <mount-point>\n\
         \x20 {program} mount-helper <storage.moss> <mount-point>\n\
         \x20 {program} create <storage.moss>\n\
         \x20 {program} clean <storage.moss>\n\
         \x20 {program} inspect <storage.moss>\n\n\
         Run without arguments to launch the TUI.\n\
         mount-helper is used internally by the TUI to mount via sudo.\n\n\
         \x20 Attach  -> Mount <storage.moss> at <mount-point> as a drive\n\
         \x20 Create  -> Create a new <storage.moss>\n\
         \x20 Clean   -> Compact <storage.moss> discarding orphaned data\n\
         \x20 Inspect -> Browse <storage.moss> in the TUI\n\
         \x20 TUI     -> Full interactive dashboard"
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let program = args.first().map(String::as_str).unwrap_or("moss");

    if args.len() < 2 || args[1] == "tui" {
        return tui::run();
    }

    let command = args[1].as_str();
    let storage_path = args.get(2).map(String::as_str).unwrap_or("");

    match command {
        "create" => {
            if storage_path.is_empty() {
                eprintln!("Error: missing storage path");
                print_usage(program);
                return Ok(());
            }
            if Path::new(storage_path).exists() {
                eprintln!("Error: '{}' already exists.", storage_path);
                return Ok(());
            }
            println!("[Moss] Creating '{}'...", storage_path);
            storage::Moss::create(storage_path)?;
            println!("[Moss] Done.");
        }
        "clean" => {
            if storage_path.is_empty() || !Path::new(storage_path).exists() {
                eprintln!("Error: storage '{}' not found.", storage_path);
                return Ok(());
            }
            println!("[Moss] Opening '{}'...", storage_path);
            let mut moss = storage::Moss::open(storage_path)?;
            let count = moss.entries().count();
            let old_len = std::fs::metadata(storage_path)
                .map(|m| m.len())
                .unwrap_or(0);
            println!(
                "[Moss] Compacting {} entries ({} bytes)...",
                count, old_len
            );
            moss.compact()?;
            let new_len = std::fs::metadata(storage_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let saved = old_len.saturating_sub(new_len);
            println!(
                "[Moss] Done. {} → {} bytes (freed {})",
                old_len, new_len, saved
            );
        }
        "inspect" => {
            if storage_path.is_empty() || !Path::new(storage_path).exists() {
                eprintln!("Error: storage '{}' not found.", storage_path);
                return Ok(());
            }
            tui::run()?;
        }
        "mount-helper" => {
            let mountpoint = args.get(3).map(String::as_str).unwrap_or("");
            if storage_path.is_empty() || mountpoint.is_empty() {
                eprintln!("Error: missing storage path or mount point");
                print_usage(program);
                return Ok(());
            }
            mount_storage(storage_path, mountpoint)?;
        }
        "attach" => {
            let mountpoint = args.get(3).map(String::as_str).unwrap_or("");
            if storage_path.is_empty() || mountpoint.is_empty() {
                eprintln!("Error: missing storage path or mount point");
                print_usage(program);
                return Ok(());
            }
            if !Path::new(storage_path).exists() {
                eprintln!("Error: '{}' not found.", storage_path);
                return Ok(());
            }
            mount_storage(storage_path, mountpoint)?;
        }
        _ => {
            eprintln!("Unknown command '{}'.", command);
            print_usage(program);
        }
    }

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn mount_storage(storage_path: &str, mountpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let storage = storage::Moss::open(storage_path)?;
    let fs = fuse_driver::MossFS::new(storage);
    let vol_name = Path::new(storage_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "moss".to_owned());
    let options = vec![
        fuser::MountOption::FSName(vol_name),
        fuser::MountOption::Subtype("moss".to_owned()),
        fuser::MountOption::RW,
        fuser::MountOption::AutoUnmount,
        fuser::MountOption::AllowOther,
    ];
    println!("[Moss] Mounting '{}' at '{}'...", storage_path, mountpoint);
    fuser::mount2(fs, mountpoint, &options)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn mount_storage(storage_path: &str, mountpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let storage = storage::Moss::open(storage_path)?;
    println!("[Moss] Mounting '{}' at '{}' using Dokany...", storage_path, mountpoint);
    windows_driver::mount(storage, mountpoint)?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
fn mount_storage(_storage_path: &str, _mountpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Mounting is not supported on this platform. Use the TUI to browse storages.");
    Ok(())
}

