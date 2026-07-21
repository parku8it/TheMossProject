use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io,
    path::{Path, PathBuf},
};
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::process::Command;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

use serde::{Deserialize, Serialize};

use crate::storage::{EntryKind, IndexEntry, Moss};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub path: String,
    pub mount_point: String,
    pub auto_mount: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub storages: Vec<StorageConfig>,
}

impl Config {
    fn load() -> Self {
        let config_path = config_file_path();
        fs::read_to_string(config_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| Self {
                storages: Vec::new(),
            })
    }

    fn save(&self) {
        let config_path = config_file_path();
        if let Some(parent) = config_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&config_path, json);
        }
    }
}

fn config_file_path() -> PathBuf {

    let home = if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Some(home_dir) = home_for_user(&sudo_user) {
            home_dir
        } else {
            std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned())
        }
    } else {
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned())
    };
    PathBuf::from(home).join(".config").join("moss").join("config.json")
}

fn home_for_user(username: &str) -> Option<String> {

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let name = std::ffi::CString::new(username).ok()?;
        let buf_size = 4096;
        let mut buf = vec![0u8; buf_size];
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut result = std::ptr::null_mut();
        let ret = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                pwd.as_mut_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf_size,
                &mut result,
            )
        };
        if ret == 0 && !result.is_null() {
            let pw = unsafe { &*result };
            let home_cstr = unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) };
            return home_cstr.to_str().ok().map(|s| s.to_owned());
        }
    }

    let home = format!("/home/{username}");
    if Path::new(&home).exists() {
        Some(home)
    } else {
        None
    }
}

struct StorageEntry {
    config: StorageConfig,
    status: StorageStatus,
}

#[derive(Clone, Copy, PartialEq)]
enum StorageStatus {
    Closed,
    Open,
    Mounted,
}

fn format_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = size as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", size as u64, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

#[derive(Clone)]
struct TreeNode {
    name: String,
    path: String,
    kind: EntryKind,
    depth: usize,
}

fn build_tree(entries: &BTreeMap<String, IndexEntry>) -> Vec<TreeNode> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut sorted_paths: Vec<&String> = entries.keys().collect();
    sorted_paths.sort();

    let mut added = std::collections::HashSet::new();

    for full_path in sorted_paths {
        let parts: Vec<&str> = full_path.split('/').collect();
        let mut current = String::new();

        for (i, part) in parts.iter().enumerate() {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);

            if added.contains(&current) {
                continue;
            }
            added.insert(current.clone());

            let kind = entries.get(&current).map(|e| e.kind).unwrap_or(EntryKind::Directory);

            nodes.push(TreeNode {
                name: part.to_string(),
                path: current.clone(),
                kind,
                depth: i,
            });
        }
    }

    nodes
}

struct InputDialog {
    prompt: String,
    buffer: String,
    visible: bool,
}

impl InputDialog {
    fn new() -> Self {
        Self {
            prompt: String::new(),
            buffer: String::new(),
            visible: false,
        }
    }

    fn show(&mut self, prompt: &str, default: &str) {
        self.prompt = prompt.to_owned();
        self.buffer = default.to_owned();
        self.visible = true;
    }

    fn hide(&mut self) {
        self.visible = false;
    }
}

#[derive(Clone)]
enum ConfirmKind {
    DeleteStorage(usize),
    OverwriteCreate(String),
}

#[derive(PartialEq)]
enum Screen {
    Dashboard,
    Browser,
    Settings,
}

#[derive(Clone)]
struct ConfirmState {
    kind: ConfirmKind,
    stage: u8,
}

pub struct TuiApp {
    config: Config,
    storages: Vec<StorageEntry>,
    open_storage: Option<Moss>,
    open_path: String,
    entries: BTreeMap<String, IndexEntry>,
    screen: Screen,
    selected_index: usize,
    file_list: Vec<TreeNode>,
    file_selected: usize,
    scroll_offset: usize,
    preview: String,
    preview_scroll: usize,
    status: String,
    input: InputDialog,
    filter: String,
    confirm: Option<ConfirmState>,
    should_quit: bool,
    mounted_count: usize,
    mount_pids: std::collections::HashMap<usize, u32>,
}

impl TuiApp {
    pub fn new() -> Self {
        let config = Config::load();
        let storages = config
            .storages
            .iter()
            .map(|cfg| StorageEntry {
                config: cfg.clone(),
                status: StorageStatus::Closed,
            })
            .collect();

        Self {
            config,
            storages,
            open_storage: None,
            open_path: String::new(),
            entries: BTreeMap::new(),
            screen: Screen::Dashboard,
            selected_index: 0,
            file_list: Vec::new(),
            file_selected: 0,
            scroll_offset: 0,
            preview: String::new(),
            preview_scroll: 0,
            status: String::from("Welcome to Moss Project"),
            input: InputDialog::new(),
            filter: String::new(),
            confirm: None,
            should_quit: false,
            mounted_count: 0,
            mount_pids: HashMap::new(),
        }
    }

    fn open_storage_at(&mut self, index: usize) {
        let path = self.storages[index].config.path.clone();
        match Moss::open(&path) {
            Ok(storage) => {
                let all_entries: BTreeMap<String, IndexEntry> = storage
                    .entries()
                    .map(|e| (e.virtual_path.clone(), e.clone()))
                    .collect();
                self.entries = all_entries;
                self.file_list = build_tree(&self.entries);
                self.file_selected = 0;
                self.scroll_offset = 0;
                self.preview.clear();
                self.open_storage = Some(storage);
                self.open_path = path.clone();
                self.storages[index].status = StorageStatus::Open;
                self.screen = Screen::Browser;
                self.filter.clear();
                self.status = format!("Opened {path}");
            }
            Err(e) => {
                self.status = format!("Failed to open: {e}");
            }
        }
    }

    fn close_storage(&mut self) {
		let open_path = self.open_path.clone();

		if let Some(mut storage) = self.open_storage.take() {
			let _ = storage.sync();
		}

		self.entries.clear();
		self.file_list.clear();
		self.preview.clear();
		self.open_path.clear();

		for storage in &mut self.storages {
			if storage.config.path == open_path
				&& storage.status != StorageStatus::Mounted
			{
				storage.status = StorageStatus::Closed;
			}
		}

		self.screen = Screen::Dashboard;
		self.status = String::from("Storage closed");
	}

    fn do_create_storage(&mut self, path: &str) {
        match Moss::create(path) {
            Ok(_) => {
                let cfg = StorageConfig {
                    path: path.to_owned(),
                    mount_point: String::new(),
                    auto_mount: false,
                };
                self.config.storages.push(cfg.clone());
                self.config.save();
                self.storages.push(StorageEntry {
                    config: cfg,
                    status: StorageStatus::Closed,
                });
                self.status = format!("Created storage at {path}");
            }
            Err(e) => {
                self.status = format!("Failed to create: {e}");
            }
        }
    }

    fn create_storage(&mut self, path: &str) {
        if Path::new(path).exists() {
            self.confirm = Some(ConfirmState {
                kind: ConfirmKind::OverwriteCreate(path.to_owned()),
                stage: 1,
            });
            self.status = "File already exists. Overwrite? (y/n) — stage 1 of 2".to_string();
        } else {
            self.do_create_storage(path);
        }
    }

    fn delete_storage_from_config(&mut self, index: usize) {
        if index < self.storages.len() {
            let path = self.storages[index].config.path.clone();
            self.config.storages.remove(index);
            self.config.save();
            self.storages.remove(index);
            if self.selected_index >= self.storages.len() && !self.storages.is_empty() {
                self.selected_index = self.storages.len() - 1;
            }
            self.status = format!("Removed {path} from config");
        }
    }

    fn import_file(&mut self, real_path: &str, virtual_path: &str) {
        let vpath = virtual_path.trim_start_matches('/');
        if vpath.is_empty() {
            self.status = String::from("Invalid virtual path");
            return;
        }
        match fs::read(real_path) {
            Ok(data) => {
                if let Some(ref mut storage) = self.open_storage {
                    match storage.write_file(vpath, &data) {
                        Ok(_) => {
                            let all_entries: BTreeMap<String, IndexEntry> = storage
                                .entries()
                                .map(|e| (e.virtual_path.clone(), e.clone()))
                                .collect();
                            self.entries = all_entries;
                            self.file_list = build_tree(&self.entries);
                            self.status = format!("Imported {real_path}");
                        }
                        Err(e) => {
                            self.status = format!("Failed to import: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                self.status = format!("Failed to read {real_path}: {e}");
            }
        }
    }

    fn export_file(&mut self, virtual_path: &str, dest_path: &str) {
        if let Some(ref mut storage) = self.open_storage {
            match storage.read_file(virtual_path) {
                Ok(data) => {
                    match fs::write(dest_path, &data) {
                        Ok(_) => {
                            self.status = format!("Exported to {dest_path}");
                        }
                        Err(e) => {
                            self.status = format!("Failed to write {dest_path}: {e}");
                        }
                    }
                }
                Err(e) => {
                    self.status = format!("Failed to read: {e}");
                }
            }
        }
    }

    fn delete_item(&mut self, path: &str) {
        if let Some(ref mut storage) = self.open_storage {
            match storage.remove_prefix(path) {
                Ok(n) if n > 0 => {
                    let all_entries: BTreeMap<String, IndexEntry> = storage
                        .entries()
                        .map(|e| (e.virtual_path.clone(), e.clone()))
                        .collect();
                    self.entries = all_entries;
                    self.file_list = build_tree(&self.entries);
                    if self.file_selected >= self.file_list.len() {
                        self.file_selected = self.file_list.len().saturating_sub(1);
                    }
                    self.preview.clear();
                    self.status = format!("Deleted {n} item(s)");
                }
                Ok(_) => {
                    self.status = format!("Nothing to delete at {path}");
                }
                Err(e) => {
                    self.status = format!("Failed to delete: {e}");
                }
            }
        }
    }

    fn rename_item(&mut self, old_path: &str, new_path: &str) {
        let new_path = new_path.trim_start_matches('/');
        if new_path.is_empty() {
            self.status = String::from("Invalid new path");
            return;
        }
        if let Some(ref mut storage) = self.open_storage {
            match storage.rename_prefix(old_path, new_path) {
                Ok(_) => {
                    let all_entries: BTreeMap<String, IndexEntry> = storage
                        .entries()
                        .map(|e| (e.virtual_path.clone(), e.clone()))
                        .collect();
                    self.entries = all_entries;
                    self.file_list = build_tree(&self.entries);
                    self.status = format!("Renamed to {new_path}");
                }
                Err(e) => {
                    self.status = format!("Failed to rename: {e}");
                }
            }
        }
    }

    fn update_preview(&mut self) {
        self.preview.clear();
        self.preview_scroll = 0;

        if self.file_list.is_empty() || self.file_selected >= self.file_list.len() {
            return;
        }

        let node = &self.file_list[self.file_selected];
        let entry = self.entries.get(&node.path);

        let mut text = String::new();
        text.push_str(&format!("Path: {}\n", node.path));
        text.push_str(&format!(
            "Type: {}\n",
            match node.kind {
                EntryKind::File => "file",
                EntryKind::Directory => "directory",
                EntryKind::Symlink => "symlink",
            }
        ));

        if let Some(entry) = entry {
            text.push_str(&format!("Size: {}\n", format_size(entry.block_length)));
            text.push_str(&format!("Mode: {:04o}\n", entry.mode));
        } else {
            text.push_str("Size: 0 B\n");
        }

        if node.kind == EntryKind::Directory {
            text.push_str("\n[directory]");
            self.preview = text;
            return;
        }

        if let Some(ref mut storage) = self.open_storage {
            match storage.read_file(&node.path) {
                Ok(data) => {
                    text.push_str(&format!("\n--- content ({} bytes) ---\n\n", data.len()));
                    if data.is_empty() {
                        text.push_str("[empty]");
                    } else if let Ok(content) = String::from_utf8(data.clone()) {
                        let lines: Vec<&str> = content.lines().collect();
                        let max_lines = 500;
                        let truncated = lines.len() > max_lines;
                        for line in lines.iter().take(max_lines) {
                            text.push_str(line);
                            text.push('\n');
                        }
                        if truncated {
                            text.push_str(&format!("... {} more lines", lines.len() - max_lines));
                        }
                    } else {
                        let hex = hex_preview(&data, 256);
                        text.push_str(&format!("[binary data]\n\n{hex}"));
                    }
                }
                Err(e) => {
                    text.push_str(&format!("\nError: {e}"));
                }
            }
        }

        self.preview = text;
    }

    fn mount_storage(&mut self, index: usize, mount_point: &str) {
        let path = self.storages[index].config.path.clone();
        let mp = mount_point.to_owned();

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let mp_for_direct = mp.clone();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let storage = Moss::open(&path).ok()?;
                let fs = crate::fuse_driver::MossFS::new(storage);
                let vol_name = Path::new(&path)
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
                std::thread::spawn(move || {
                    let _ = fuser::mount2(fs, &mp_for_direct, &options);
                });
                Some(())
            })) {
                Ok(Some(())) => {
                    self.storages[index].status = StorageStatus::Mounted;
                    self.mounted_count += 1;
                    self.status = format!("Mounted at {mount_point}");
                    return;
                }
                _ => {}
            }
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
		{
			if let Err(error) = fs::create_dir_all(&mp) {
				self.status = format!("Failed to create mount point: {error}");
				return;
			}

			let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("moss"));

			let mut command = Command::new(&binary);
			command
				.arg("mount-helper")
				.arg(&path)
				.arg(&mp);

			match command.spawn() {
				Ok(child) => {
					let pid = child.id();
					self.mount_pids.insert(index, pid);
					std::mem::forget(child);

					self.storages[index].status = StorageStatus::Mounted;
					self.storages[index].config.mount_point = mp.clone();

					if index < self.config.storages.len() {
						self.config.storages[index].mount_point = mp.clone();
						self.config.save();
					}

					self.mounted_count += 1;
					self.status = format!("Mount helper started at {mp} (pid {pid})");
				}
				Err(error) => {
					self.status = format!("Mount failed: {error}");
				}
			}
		}

        #[cfg(target_os = "windows")]
        {
            match Moss::open(&path) {
                Ok(storage) => {
                    let mp_clone = mp.clone();
                    std::thread::spawn(move || {
                        let _ = crate::windows_driver::mount(storage, &mp_clone);
                    });
                    self.storages[index].status = StorageStatus::Mounted;
                    self.storages[index].config.mount_point = mp.clone();
                    if index < self.config.storages.len() {
                        self.config.storages[index].mount_point = mp.clone();
                        self.config.save();
                    }
                    self.mounted_count += 1;
                    self.status = format!("Mounted at {mount_point}");
                }
                Err(e) => {
                    self.status = format!("Failed to open storage: {e}");
                }
            }
        }
    }

    fn unmount_storage(&mut self, index: usize) {
		if index >= self.storages.len() {
			self.status = String::from("Invalid storage index");
			return;
		}

		let mount_point = self.storages[index].config.mount_point.clone();

		#[cfg(any(target_os = "linux", target_os = "android"))]
		{
			let pid = self.mount_pids.get(&index).copied();
			if !mount_point.is_empty() {
				let unmounted = Command::new("fusermount3")
					.args(["-u", &mount_point])
					.status()
					.map(|status| status.success())
					.unwrap_or(false)
					|| Command::new("fusermount")
						.args(["-u", &mount_point])
						.status()
						.map(|status| status.success())
						.unwrap_or(false);

				if !unmounted {
					let lazy_unmounted = Command::new("fusermount3")
						.args(["-uz", &mount_point])
						.status()
						.map(|status| status.success())
						.unwrap_or(false)
						|| Command::new("fusermount")
							.args(["-uz", &mount_point])
							.status()
							.map(|status| status.success())
							.unwrap_or(false);

					if !lazy_unmounted {
						self.status = format!("Failed to unmount {mount_point}");
						return;
					}
				}
			}

			if let Some(pid) = pid {
				let mut exited = false;
				for _ in 0..50 {
					let still_running = Command::new("kill")
						.args(["-0", &pid.to_string()])
						.status()
						.map(|status| status.success())
						.unwrap_or(false);
					if !still_running {
						exited = true;
						break;
					}
					std::thread::sleep(std::time::Duration::from_millis(100));
				}
				if !exited {
					let _ = Command::new("kill")
						.args(["-TERM", &pid.to_string()])
						.status();
				}
			}
		}

		#[cfg(target_os = "windows")]
		if !mount_point.is_empty() {
			if let Ok(wide) = widestring::U16CString::from_str(&mount_point) {
				let _ = dokan::unmount(&wide);
			}
		}

		self.mount_pids.remove(&index);
		self.storages[index].status = StorageStatus::Closed;
		self.mounted_count = self.mounted_count.saturating_sub(1);
		self.status = format!("Unmounted {mount_point}");
	}

    fn filtered_file_list(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.file_list.len()).collect();
        }
        let lower = self.filter.to_lowercase();
        self.file_list
            .iter()
            .enumerate()
            .filter(|(_, n)| n.path.to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect()
    }
}

fn hex_preview(data: &[u8], limit: usize) -> String {
    let mut output = String::new();
    for (line, chunk) in data.iter().take(limit).collect::<Vec<_>>().chunks(16).enumerate() {
        output.push_str(&format!("{:08x}  ", line * 16));
        for byte in chunk {
            output.push_str(&format!("{byte:02x} "));
        }
        output.push('\n');
    }
    if data.len() > limit {
        output.push_str("... preview truncated ...");
    }
    output
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let _guard = TermGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = TuiApp::new();
    let res = run_app(&mut terminal, &mut app);
    terminal.show_cursor()?;

    if let Err(e) = &res {
        eprintln!("Error: {e}");
    }

    res?;
    Ok(())
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut TuiApp) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui(frame, app))?;

        if app.should_quit {
            break;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if app.input.visible {
                match key.code {
                    KeyCode::Esc => {
                        app.input.hide();
                    }
                    KeyCode::Enter => {
                        let input = app.input.buffer.clone();
                        let prompt = app.input.prompt.clone();
                        app.input.hide();

                        match app.screen {
                            Screen::Dashboard => {
                                if prompt.starts_with("Create:") {
                                    if !input.is_empty() {
                                        app.create_storage(&input);
                                    }
                                } else if prompt.starts_with("Open:") || prompt.starts_with("Import:") {
                                    if !input.is_empty() && Path::new(&input).exists() {
                                        let known = app.config.storages.iter().any(|s| s.path == input);
                                        if !known {
                                            let cfg = StorageConfig {
                                                path: input.clone(),
                                                mount_point: String::new(),
                                                auto_mount: false,
                                            };
                                            app.config.storages.push(cfg.clone());
                                            app.config.save();
                                            app.storages.push(StorageEntry {
                                                config: cfg,
                                                status: StorageStatus::Closed,
                                            });
                                        }
                                        let idx = app.storages.iter().position(|s| s.config.path == input);
                                        if let Some(idx) = idx {
                                            app.selected_index = idx;
                                            app.open_storage_at(idx);
                                        }
                                    } else {
                                        app.status = "File not found".to_string();
                                    }
                                } else if prompt.starts_with("Mount:") {
                                    if !input.is_empty() {
                                        app.mount_storage(app.selected_index, &input);
                                    }
                                }
                            }
                            Screen::Browser => {
                                if prompt.starts_with("Import") {
                                    let parts: Vec<&str> = input.splitn(2, ' ').collect();
                                    if parts.len() == 2 {
                                        app.import_file(parts[0], parts[1]);
                                    } else if parts.len() == 1 {
                                        let path = parts[0];
                                        let name = Path::new(path)
                                            .file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_default();
                                        app.import_file(path, &name);
                                    } else {
                                        app.status =
                                            String::from("Usage: <real-path> [virtual-path]");
                                    }
                                } else if prompt.starts_with("Export") {
                                    if !input.is_empty() && app.file_selected < app.file_list.len() {
                                        let src = app.file_list[app.file_selected].path.clone();
                                        app.export_file(&src, &input);
                                    }
                                } else if prompt.starts_with("Rename") {
                                    if !input.is_empty() && app.file_selected < app.file_list.len() {
                                        let old = app.file_list[app.file_selected].path.clone();
                                        app.rename_item(&old, &input);
                                    }
                                } else if prompt.starts_with("Filter") {
                                    app.filter = input;
                                    app.file_selected = 0;
                                }
                            }
                            Screen::Settings => {
                                if prompt.starts_with("Path:") {
                                    if let Some((p, _)) = input.split_once(' ') {
                                        let cfg = StorageConfig {
                                            path: p.to_owned(),
                                            mount_point: String::new(),
                                            auto_mount: false,
                                        };
                                        app.config.storages.push(cfg.clone());
                                        app.config.save();
                                        app.storages.push(StorageEntry {
                                            config: cfg,
                                            status: StorageStatus::Closed,
                                        });
                                        app.status = format!("Added {p}");
                                    }
                                } else if prompt.starts_with("Mount point") {
                                    let idx = app.selected_index;
                                    if idx < app.config.storages.len() {
                                        app.config.storages[idx].mount_point = input.clone();
                                        app.config.save();
                                        if idx < app.storages.len() {
                                            app.storages[idx].config.mount_point = input;
                                        }
                                        app.status = format!("Set mount point for {}", app.config.storages[idx].path);
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        app.input.buffer.push(c);
                    }
                    KeyCode::Backspace => {
                        app.input.buffer.pop();
                    }
                    KeyCode::Delete => {
                        app.input.buffer.clear();
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(confirm) = app.confirm.clone() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        if confirm.stage >= 2 {
                            match &confirm.kind {
                                ConfirmKind::DeleteStorage(idx) => {
                                    app.delete_storage_from_config(*idx);
                                }
                                ConfirmKind::OverwriteCreate(ref path) => {
                                    app.do_create_storage(path);
                                }
                            }
                            app.confirm = None;
                        } else {
                            let next = ConfirmState {
                                stage: confirm.stage + 1,
                                ..confirm
                            };
                            let msg = match &next.kind {
                                ConfirmKind::DeleteStorage(_) => {
                                    "Are you really sure? This cannot be undone. (y/n)".to_string()
                                }
                                ConfirmKind::OverwriteCreate(_) => {
                                    "Are you really sure? This will destroy existing data. (y/n) — stage 2 of 2".to_string()
                                }
                            };
                            app.confirm = Some(next);
                            app.status = msg;
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        app.confirm = None;
                        app.status = "Cancelled.".to_string();
                    }
                    _ => {}
                }
                continue;
            }

            match app.screen {
                Screen::Dashboard => handle_dashboard_key(app, key),
                Screen::Browser => handle_browser_key(app, key),
                Screen::Settings => handle_settings_key(app, key),
            }
        }
    }

    if let Some(ref mut storage) = app.open_storage {
        let _ = storage.sync();
    }
    app.config.save();

    Ok(())
}

fn next_selectable(n: usize, current: usize, dir: i32) -> usize {
    if n == 0 {
        return 0;
    }
    let current = current as i32;
    let next = (current + dir).rem_euclid(n as i32);
    next as usize
}

fn handle_dashboard_key(app: &mut TuiApp, key: crossterm::event::KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::Char('Q') => {
            app.should_quit = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected_index = next_selectable(app.storages.len(), app.selected_index, -1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected_index = next_selectable(app.storages.len(), app.selected_index, 1);
        }
        KeyCode::Enter | KeyCode::Char('o') => {
            if !app.storages.is_empty() {
                let idx = app.selected_index.min(app.storages.len() - 1);
                if Path::new(&app.storages[idx].config.path).exists() {
                    app.open_storage_at(idx);
                } else {
                    app.status = "File does not exist".to_string();
                }
            }
        }
        KeyCode::Char('c') => {
            app.input.show("Create: enter path for new .moss file", "");
        }
        KeyCode::Char('O') => {
            app.input.show("Open: enter path to existing .moss file", "");
        }
        KeyCode::Char('i') => {
            app.input.show("Import: enter path to existing .moss file", "");
        }
        KeyCode::Char('m') => {
            if !app.storages.is_empty() {
                let idx = app.selected_index.min(app.storages.len() - 1);
                let default_mp = app.storages[idx].config.mount_point.clone();
                app.input.show("Mount: enter mount point", &default_mp);
            }
        }
        KeyCode::Char('u') => {
            if !app.storages.is_empty() {
                let idx = app.selected_index.min(app.storages.len() - 1);
                if app.storages[idx].status == StorageStatus::Mounted {
                    app.unmount_storage(idx);
                } else {
                    app.status = "Not mounted".to_string();
                }
            }
        }
        KeyCode::Char('d') => {
            if !app.storages.is_empty() {
                let idx = app.selected_index.min(app.storages.len() - 1);
                app.confirm = Some(ConfirmState {
                    kind: ConfirmKind::DeleteStorage(idx),
                    stage: 1,
                });
                app.status = "Delete this entry? (y/n)".to_string();
            }
        }
        KeyCode::Char('s') => {
            app.screen = Screen::Settings;
            app.selected_index = 0;
            app.status = "Settings - manage auto-mount storages".to_string();
        }
        KeyCode::Char('r') => {
            let config = Config::load();
            for storage in &mut app.storages {
                if let Some(cfg) = config.storages.iter().find(|c| c.path == storage.config.path) {
                    storage.config.mount_point = cfg.mount_point.clone();
                    storage.config.auto_mount = cfg.auto_mount;
                }
            }
            app.status = "Config refreshed".to_string();
        }
        _ => {}
    }
}

fn handle_browser_key(app: &mut TuiApp, key: crossterm::event::KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.close_storage();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let filtered = app.filtered_file_list();
            app.file_selected = if filtered.is_empty() {
                0
            } else {
                let pos = filtered.iter().position(|&i| i == app.file_selected);
                match pos {
                    Some(p) if p > 0 => filtered[p - 1],
                    _ => filtered[filtered.len() - 1],
                }
            };
            app.update_preview();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let filtered = app.filtered_file_list();
            app.file_selected = if filtered.is_empty() {
                0
            } else {
                let pos = filtered.iter().position(|&i| i == app.file_selected);
                match pos {
                    Some(p) if p + 1 < filtered.len() => filtered[p + 1],
                    _ => filtered[0],
                }
            };
            app.update_preview();
        }
        KeyCode::PageDown => {
            let filtered = app.filtered_file_list();
            if !filtered.is_empty() {
                let pos = filtered.iter().position(|&i| i == app.file_selected).unwrap_or(0);
                let new_pos = (pos + 20).min(filtered.len() - 1);
                app.file_selected = filtered[new_pos];
            }
            app.update_preview();
        }
        KeyCode::PageUp => {
            let filtered = app.filtered_file_list();
            if !filtered.is_empty() {
                let pos = filtered.iter().position(|&i| i == app.file_selected).unwrap_or(0);
                let new_pos = pos.saturating_sub(20);
                app.file_selected = filtered[new_pos];
            }
            app.update_preview();
        }
        KeyCode::Home => {
            if !app.file_list.is_empty() {
                app.file_selected = 0;
            }
            app.update_preview();
        }
        KeyCode::End => {
            if !app.file_list.is_empty() {
                app.file_selected = app.file_list.len() - 1;
            }
            app.update_preview();
        }
        KeyCode::Enter => {
            if app.file_selected < app.file_list.len() {
                let node = &app.file_list[app.file_selected];
                if node.kind == EntryKind::Directory {

                    let prefix = format!("{}/", node.path);
                    if app.filter.is_empty() || !app.filter.starts_with(&prefix) {
                        app.filter = prefix;
                        app.file_selected = 0;
                    } else {
                        app.filter.clear();
                    }
                } else {
                    app.update_preview();
                }
            }
        }
        KeyCode::Char('i') => {
            app.input.show("Import: <real-path> [virtual-path]", "");
        }
        KeyCode::Char('e') => {
            if app.file_selected < app.file_list.len() {
                let node = &app.file_list[app.file_selected];
                if node.kind != EntryKind::Directory {
                    let default = node.name.clone();
                    app.input.show("Export: enter destination path", &default);
                } else {
                    app.status = "Cannot export a directory".to_string();
                }
            }
        }
        KeyCode::Char('d') => {
            if app.file_selected < app.file_list.len() {
                let del_path = app.file_list[app.file_selected].path.clone();
                if del_path.is_empty() {
                    app.status = "Cannot delete root".to_string();
                } else {
                    app.delete_item(&del_path);
                }
            }
        }
        KeyCode::Char('r') => {
            if app.file_selected < app.file_list.len() {
                let p = app.file_list[app.file_selected].path.clone();
                if p.is_empty() {
                    app.status = "Cannot rename root".to_string();
                } else {
                    app.input.show("Rename: enter new path", &p);
                }
            }
        }
        KeyCode::Char('f') => {
            app.input.show("Filter: enter search term", &app.filter);
        }
        KeyCode::Char('/') => {
            app.input.show("Filter:", &app.filter);
        }
        KeyCode::Char('h') | KeyCode::Char('?') => {
            app.status = "i:import e:export d:delete r:rename f:filter q:back".to_string();
        }
        _ => {}
    }
}

fn handle_settings_key(app: &mut TuiApp, key: crossterm::event::KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('b') => {
            app.config.save();
            app.screen = Screen::Dashboard;
            app.status = "Settings saved".to_string();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected_index = next_selectable(app.config.storages.len(), app.selected_index, -1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected_index = next_selectable(app.config.storages.len(), app.selected_index, 1);
        }
        KeyCode::Char('a') => {
            if app.config.storages.is_empty() {
                app.input.show("Path: enter path to .moss file", "");
            } else {
                app.status = "Storages already configured".to_string();
            }
        }
        KeyCode::Char('d') => {
            if !app.config.storages.is_empty() {
                let idx = app.selected_index.min(app.config.storages.len() - 1);
                let path = app.config.storages[idx].path.clone();
                app.config.storages.remove(idx);
                app.config.save();
                app.storages.retain(|s| s.config.path != path);
                if app.selected_index >= app.config.storages.len() {
                    app.selected_index = app.config.storages.len().saturating_sub(1);
                }
                app.status = format!("Removed {path}");
            }
        }
        KeyCode::Char('m') => {
            if !app.config.storages.is_empty() {
                let idx = app.selected_index.min(app.config.storages.len() - 1);
                let default = app.config.storages[idx].mount_point.clone();
                app.input.show(
                    &format!("Mount point for {}:", app.config.storages[idx].path),
                    &default,
                );

                app.status = format!("Editing mount point for {}", app.config.storages[idx].path);
            }
        }
        KeyCode::Char('t') => {
            if !app.config.storages.is_empty() {
                let idx = app.selected_index.min(app.config.storages.len() - 1);
                let new_val = !app.config.storages[idx].auto_mount;
                let s_path = app.config.storages[idx].path.clone();
                app.config.storages[idx].auto_mount = new_val;
                app.config.save();
                if idx < app.storages.len() {
                    app.storages[idx].config.auto_mount = new_val;
                }
                app.status = if new_val {
                    format!("Auto-mount enabled for {s_path}")
                } else {
                    format!("Auto-mount disabled for {s_path}")
                };
            }
        }
        _ => {}
    }
}

fn ui(frame: &mut Frame, app: &mut TuiApp) {
    match app.screen {
        Screen::Dashboard => draw_dashboard(frame, app),
        Screen::Browser => draw_browser(frame, app),
        Screen::Settings => draw_settings(frame, app),
    }

    if app.input.visible {
        draw_input_dialog(frame, app);
    }

    if let Some(confirm) = &app.confirm {
        let msg = match &confirm.kind {
            ConfirmKind::DeleteStorage(_) => {
                if confirm.stage >= 2 {
                    "Are you really sure? This cannot be undone. (y/n)"
                } else {
                    "Delete this entry? (y/n)"
                }
            }
            ConfirmKind::OverwriteCreate(_) => match confirm.stage {
                1 => "File already exists. Overwrite? (y/n) — stage 1 of 2",
                _ => "Are you really sure? This will destroy existing data. (y/n) — stage 2 of 2",
            },
        };
        draw_confirm_dialog(frame, msg);
    }
}

fn draw_dashboard(frame: &mut Frame, app: &mut TuiApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let title = Line::from(vec![
        Span::styled(" Moss Project ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(concat!("0.2.", env!("BUILD_NUMBER")), Style::default().fg(Color::DarkGray)),
        Span::raw("  —  "),
        Span::styled("Mountable Organized Secure Storage", Style::default().fg(Color::Cyan)),
    ]);
    let title_widget = Paragraph::new(title).style(Style::default().bg(Color::Black));
    frame.render_widget(title_widget, chunks[0]);

    let items: Vec<ListItem> = app
        .storages
        .iter()
        .map(|entry| {
            let exists = Path::new(&entry.config.path).exists();
            let status_str = match entry.status {
                StorageStatus::Closed if exists => "closed",
                StorageStatus::Closed => "missing",
                StorageStatus::Open => "open",
                StorageStatus::Mounted => "mounted",
            };
            let status_color = match entry.status {
                StorageStatus::Closed if exists => Color::DarkGray,
                StorageStatus::Closed => Color::Red,
                StorageStatus::Open => Color::Green,
                StorageStatus::Mounted => Color::Yellow,
            };
            let auto = if entry.config.auto_mount { " [auto]" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:43} ", entry.config.path), Style::default()),
                Span::styled(
                    format!("[{status_str:7}]{auto}"),
                    Style::default().fg(status_color),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Storages ")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black)),
        )
        .style(Style::default().bg(Color::Black))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(if app.storages.is_empty() {
        None
    } else {
        Some(app.selected_index.min(app.storages.len() - 1))
    });

    frame.render_stateful_widget(list, chunks[1], &mut state);

    let help_text = format!(
        " [O]pen  [C]reate  [M]ount  [U]nmount  [D]elete  [S]ettings  [R]efresh  [Q]uit  [I]mport   |   {}",
        app.status
    );
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::White).bg(Color::Blue));
    frame.render_widget(help, chunks[2]);

    let kb_hint = Line::from(vec![
        Span::styled(" ↑↓ nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter open  ", Style::default().fg(Color::DarkGray)),
        Span::styled("c create  ", Style::default().fg(Color::DarkGray)),
        Span::styled("O open-path  ", Style::default().fg(Color::DarkGray)),
        Span::styled("m mount  ", Style::default().fg(Color::DarkGray)),
        Span::styled("u unmount  ", Style::default().fg(Color::DarkGray)),
        Span::styled("d delete  ", Style::default().fg(Color::DarkGray)),
        Span::styled("s settings  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q quit", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(
        Paragraph::new(kb_hint).style(Style::default().bg(Color::Black)),
        chunks[3],
    );
}

fn draw_browser(frame: &mut Frame, app: &mut TuiApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let path_text = format!(
        " {}  —  {} items  {}",
        app.open_path,
        app.file_list.len(),
        if app.filter.is_empty() {
            String::new()
        } else {
            format!("filter: {}", app.filter)
        }
    );
    let title = Paragraph::new(Line::from(vec![
        Span::styled(" Moss Browser ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(path_text, Style::default().fg(Color::DarkGray)),
    ]))
    .style(Style::default().bg(Color::Black));
    frame.render_widget(title, chunks[0]);

    let browser_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);

    let filtered_indices = app.filtered_file_list();
    let items: Vec<ListItem> = filtered_indices
        .iter()
        .map(|&idx| {
            let node = &app.file_list[idx];
            let indent = "  ".repeat(node.depth.min(10));
            let prefix = match node.kind {
                EntryKind::Directory => "D ",
                EntryKind::Symlink => "L ",
                EntryKind::File => "  ",
            };
            let name = if node.kind == EntryKind::Directory {
                format!("{}{}{}/", indent, prefix, node.name)
            } else {
                format!("{}{}{}", indent, prefix, node.name)
            };
            let colored = if app.filter.is_empty()
                || node.path.to_lowercase().contains(&app.filter.to_lowercase())
            {
                let color = match node.kind {
                    EntryKind::Directory => Color::Cyan,
                    EntryKind::Symlink => Color::Magenta,
                    EntryKind::File => Color::White,
                };
                ListItem::new(name).style(Style::default().fg(color))
            } else {
                ListItem::new(name)
            };
            colored
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Files ")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black)),
        )
        .style(Style::default().bg(Color::Black))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    let sel = if filtered_indices.is_empty() {
        None
    } else {
        let pos = filtered_indices
            .iter()
            .position(|&i| i == app.file_selected)
            .unwrap_or(0);
        Some(pos)
    };
    state.select(sel);
    frame.render_stateful_widget(list, browser_chunks[0], &mut state);

    if app.preview.is_empty() && app.file_selected < app.file_list.len() {
        app.update_preview();
    }

    let preview = Paragraph::new(app.preview.as_str())
        .block(
            Block::default()
                .title(" Preview ")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black)),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .wrap(Wrap { trim: false })
        .scroll((app.preview_scroll as u16, 0));
    frame.render_widget(preview, browser_chunks[1]);

    let kb = Line::from(vec![
        Span::styled(" ↑↓ nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter dir  ", Style::default().fg(Color::DarkGray)),
        Span::styled("i import  ", Style::default().fg(Color::DarkGray)),
        Span::styled("e export  ", Style::default().fg(Color::DarkGray)),
        Span::styled("d delete  ", Style::default().fg(Color::DarkGray)),
        Span::styled("r rename  ", Style::default().fg(Color::DarkGray)),
        Span::styled("/ filter  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q back", Style::default().fg(Color::DarkGray)),
        Span::raw("  |  "),
        Span::styled(&app.status, Style::default().fg(Color::Yellow)),
    ]);
    frame.render_widget(kb, chunks[2]);
}

fn draw_settings(frame: &mut Frame, app: &mut TuiApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(" Settings ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(" — auto-mount configuration", Style::default().fg(Color::DarkGray)),
    ]))
    .style(Style::default().bg(Color::Black));
    frame.render_widget(title, chunks[0]);

    let items: Vec<ListItem> = app
        .config
        .storages
        .iter()
        .map(|s| {
            let auto = if s.auto_mount { "[x]" } else { "[ ]" };
            let mp = if s.mount_point.is_empty() {
                "(no mount point)"
            } else {
                &s.mount_point
            };
            ListItem::new(Line::from(Span::raw(format!(
                " {auto}  {:40}  mount: {}",
                s.path, mp
            ))))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Auto-mount Storages ")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black)),
        )
        .style(Style::default().bg(Color::Black))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(if app.config.storages.is_empty() {
        None
    } else {
        Some(app.selected_index.min(app.config.storages.len() - 1))
    });
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let help = Paragraph::new(format!(" {}  |  [A]dd  [D]elete  [T]oggle auto  [M]ount point  [B]ack", app.status))
        .style(Style::default().fg(Color::White).bg(Color::Blue));
    frame.render_widget(help, chunks[2]);

    let kb = Line::from(vec![
        Span::styled(" ↑↓ nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("a add  ", Style::default().fg(Color::DarkGray)),
        Span::styled("d delete  ", Style::default().fg(Color::DarkGray)),
        Span::styled("t toggle  ", Style::default().fg(Color::DarkGray)),
        Span::styled("m mount point  ", Style::default().fg(Color::DarkGray)),
        Span::styled("b back", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(kb, chunks[3]);
}

fn draw_input_dialog(frame: &mut Frame, app: &mut TuiApp) {
    let area = frame.area();
    let dialog_width = area.width.min(60).max(30);
    let dialog_height = 5;
    let x = (area.width - dialog_width) / 2;
    let y = (area.height - dialog_height) / 2;

    let dialog_area = Rect::new(x, y, dialog_width, dialog_height);

    let clear = Clear;
    frame.render_widget(clear, dialog_area);

    let lines = vec![
        Line::from(Span::styled(
            &app.input.prompt,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw("")),
        Line::from(Span::styled(
            &app.input.buffer,
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            if app.input.buffer.is_empty() {
                "(type and press Enter)"
            } else {
                "Enter to confirm, Esc to cancel"
            },
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Input ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .style(Style::default().bg(Color::Black));

    frame.render_widget(paragraph, dialog_area);
}

fn draw_confirm_dialog(frame: &mut Frame, message: &str) {
    let area = frame.area();
    let dialog_width = 40;
    let dialog_height = 4;
    let x = (area.width - dialog_width) / 2;
    let y = (area.height - dialog_height) / 2;

    let dialog_area = Rect::new(x, y, dialog_width, dialog_height);
    let clear = Clear;
    frame.render_widget(clear, dialog_area);

    let lines = vec![
        Line::from(Span::styled(
            message,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw("")),
        Line::from(Span::styled(
            "y/Enter = yes, n/Esc = no",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Confirm ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .style(Style::default().bg(Color::Black));

    frame.render_widget(paragraph, dialog_area);
}

