use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    panic,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime},
};

use dokan::{
    CreateFileInfo, DiskSpaceInfo, FileInfo, FileSystemHandler, FileSystemMounter,
    FileTimeOperation, FillDataResult, FindData, MountFlags, MountOptions, OperationInfo,
    OperationResult, VolumeInfo,
};
use dokan_sys::win32::{
    FILE_CREATE, FILE_DELETE_ON_CLOSE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_IF, FILE_OVERWRITE, FILE_OVERWRITE_IF, FILE_SUPERSEDE,
};
use widestring::{U16CStr, U16CString};
use winapi::{
    shared::{ntdef::NTSTATUS, ntstatus::*},
    um::winnt,
};

use crate::storage::{EntryKind, Moss, Timestamp};

fn log_msg(msg: &str) {
    let line = format!("{}\n", msg);
    eprint!("{}", line);
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("moss.log")
    {
        let _ = f.write_all(line.as_bytes());
    }
}

macro_rules! log_op {
    ($fmt:literal $(, $arg:expr)*) => {
        log_msg(&format!($fmt $(, $arg)*))
    };
}

fn setup_panic_hook() {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        log_msg(&format!("[PANIC] {}", info));
        prev(info);
    }));
}

fn normalize_pons_path(file_name: &U16CStr) -> Result<String, NTSTATUS> {
    let wide = file_name.as_slice();
    let start = wide
        .iter()
        .position(|&c| c != '\\' as u16)
        .unwrap_or(wide.len());
    let trimmed = &wide[start..];

    let utf8 = String::from_utf16(trimmed).map_err(|_| STATUS_OBJECT_NAME_INVALID)?;
    let mut normalized = Vec::new();

    for component in utf8.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return Err(STATUS_OBJECT_PATH_NOT_FOUND),
            c if c.contains('\0') => return Err(STATUS_OBJECT_NAME_INVALID),
            c => normalized.push(c.to_owned()),
        }
    }

    Ok(normalized.join("/"))
}

fn file_name_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent_path(path: &str) -> &str {
    if path.is_empty() {
        return "";
    }
    match path.rfind('/') {
        Some(pos) => &path[..pos],
        None => "",
    }
}

fn mode_to_win_attrs(mode: u16, kind: EntryKind) -> u32 {
    let mut a = 0;
    if kind == EntryKind::Directory {
        a |= winnt::FILE_ATTRIBUTE_DIRECTORY;
    }
    if kind != EntryKind::Directory && mode & 0o222 == 0 {
        a |= winnt::FILE_ATTRIBUTE_READONLY;
    }
    if a == 0 {
        a = winnt::FILE_ATTRIBUTE_NORMAL;
    }
    a
}

fn win_attrs_to_mode(attrs: u32, kind: EntryKind) -> u16 {
    match kind {
        EntryKind::Directory => {
            if attrs & winnt::FILE_ATTRIBUTE_READONLY != 0 {
                0o555
            } else {
                0o755
            }
        }
        EntryKind::Symlink => 0o777,
        EntryKind::File => {
            if attrs & winnt::FILE_ATTRIBUTE_READONLY != 0 {
                0o444
            } else {
                0o644
            }
        }
    }
}

fn timestamp_to_system_time(ts: Timestamp) -> SystemTime {
    ts.to_system_time()
}

#[derive(Clone)]
struct FsNode {
    kind: EntryKind,
    size: u64,
    mode: u16,
    attrs: u32,
    atime: Timestamp,
    mtime: Timestamp,
    ctime: Timestamp,
}

struct Inner {
    nodes: HashMap<String, FsNode>,
    children: HashMap<String, Vec<String>>,
    write_cache: HashMap<String, Vec<u8>>,
	last_mutation: Instant,
	index_dirty: bool,
    archive: Moss,
}

impl Inner {
    fn build_tree(storage: Moss) -> Self {
        let entries: Vec<_> = storage.entries().cloned().collect();

        let mut this = Self {
			nodes: HashMap::new(),
			children: HashMap::new(),
			write_cache: HashMap::new(),
			archive: storage,
			last_mutation: Instant::now(),
			index_dirty: false,
		};

        this.nodes.insert(
            String::new(),
            FsNode {
                kind: EntryKind::Directory,
                size: 0,
                mode: 0o755,
                attrs: winnt::FILE_ATTRIBUTE_DIRECTORY,
                atime: Timestamp::now(),
                mtime: Timestamp::now(),
                ctime: Timestamp::now(),
            },
        );
        this.children.insert(String::new(), Vec::new());

        for entry in entries {
            let parts: Vec<&str> = entry.virtual_path.split('/').collect();
            let mut current = String::new();

            for (pos, part) in parts.iter().enumerate() {
                let is_last = pos + 1 == parts.len();

                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(part);

                if !this.nodes.contains_key(&current) {
                    let kind = if is_last {
                        entry.kind
                    } else {
                        EntryKind::Directory
                    };

                    let node = FsNode {
                        kind,
                        size: if is_last && kind != EntryKind::Directory {
                            entry.block_length
                        } else {
                            0
                        },
                        mode: if is_last { entry.mode } else { 0o755 },
                        attrs: mode_to_win_attrs(
                            if is_last { entry.mode } else { 0o755 },
                            kind,
                        ),
                        atime: entry.atime,
                        mtime: entry.mtime,
                        ctime: entry.ctime,
                    };

                    this.nodes.insert(current.clone(), node);
                    this.children.entry(current.clone()).or_default();

                    let p = parent_path(&current);
                    this.children.entry(p.to_owned()).or_default().push(current.clone());
                } else if is_last {
                    if let Some(node) = this.nodes.get_mut(&current) {
                        node.kind = entry.kind;
                        node.size = entry.block_length;
                        node.mode = entry.mode;
                        node.attrs = mode_to_win_attrs(entry.mode, entry.kind);
                        node.atime = entry.atime;
                        node.mtime = entry.mtime;
                        node.ctime = entry.ctime;
                    }
                }
            }
        }

        for children in this.children.values_mut() {
            children.sort();
            children.dedup();
        }

        this
    }

	fn mark_mutated(&mut self) {
		self.last_mutation = Instant::now();
		self.index_dirty = true;
	}

	fn checkpoint_if_idle(&mut self, idle_duration: Duration) {
		if !self.index_dirty || self.last_mutation.elapsed() < idle_duration {
			return;
		}

		if self.archive.checkpoint().is_ok() {
			self.index_dirty = false;
		}
	}

	fn force_sync(&mut self) -> std::io::Result<()> {
		self.archive.sync()?;
		self.index_dirty = false;
		Ok(())
	}

	fn ensure_write_cache(&mut self, path: &str) -> Result<&mut Vec<u8>, NTSTATUS> {
		if !self.write_cache.contains_key(path) {
			let data = self
				.archive
				.read_file(path)
				.map_err(|_| STATUS_INTERNAL_ERROR)?;

			self.write_cache.insert(path.to_owned(), data);
		}

		self.write_cache
			.get_mut(path)
			.ok_or(STATUS_INTERNAL_ERROR)
	}

    fn commit_write(&mut self, path: &str) -> Result<(), NTSTATUS> {
		let Some(data) = self.write_cache.remove(path) else {
			return Ok(());
		};

		let Some(node) = self.nodes.get_mut(path) else {
			self.write_cache.insert(path.to_owned(), data);
			return Err(STATUS_OBJECT_NAME_NOT_FOUND);
		};

		node.size = data.len() as u64;
		let now = Timestamp::now();
		node.mtime = now;
		node.ctime = now;

		let entry = crate::storage::IndexEntry {
			virtual_path: path.to_owned(),
			kind: node.kind,
			start_byte: 0,
			block_length: data.len() as u64,
			mode: node.mode,
			uid: 0,
			gid: 0,
			atime: node.atime,
			mtime: now,
			ctime: now,
		};

		if self.archive.upsert_entry(entry, Some(&data)).is_err() {
			self.write_cache.insert(path.to_owned(), data);
			return Err(STATUS_INTERNAL_ERROR);
		}

		self.mark_mutated();
		Ok(())
	}
}

pub struct HandleContext {
    path: String,
    delete_on_close: bool,
}

const IDLE_CHECKPOINT_DELAY: Duration = Duration::from_secs(10);

pub struct MossDokan {
    inner: Arc<Mutex<Inner>>,
    volume_name: String,
}

impl MossDokan {
    pub fn new(archive: Moss, volume_name: String) -> Self {
        let inner = Arc::new(Mutex::new(Inner::build_tree(archive)));
        let weak_inner: Weak<Mutex<Inner>> = Arc::downgrade(&inner);

        std::thread::Builder::new()
            .name("moss-windows-idle-checkpoint".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1));

                    let Some(inner) = weak_inner.upgrade() else {
                        break;
                    };

                    match inner.lock() {
                        Ok(mut inner) => {
                            inner.checkpoint_if_idle(IDLE_CHECKPOINT_DELAY);
                        }
                        Err(poisoned) => {
                            let mut inner = poisoned.into_inner();
                            inner.checkpoint_if_idle(IDLE_CHECKPOINT_DELAY);
                        }
                    };
                }
            })
            .expect("failed to start Windows Moss checkpoint thread");

        Self { inner, volume_name }
    }
}

impl<'c, 'h: 'c> FileSystemHandler<'c, 'h> for MossDokan {
    type Context = HandleContext;

    fn create_file(
        &'h self,
        file_name: &U16CStr,
        _security_context: &dokan::IO_SECURITY_CONTEXT,
        _desired_access: winnt::ACCESS_MASK,
        file_attributes: u32,
        _share_access: u32,
        create_disposition: u32,
        create_options: u32,
        _info: &mut OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<CreateFileInfo<Self::Context>> {
        let path = normalize_pons_path(file_name).map_err(|e| {
            log_op!("create_file normalize failed: {:?}", file_name);
            e
        })?;
        log_op!("create_file: path={:?} disp={:#x} opts={:#x}", path, create_disposition, create_options);
        let is_dir_request = create_options & FILE_DIRECTORY_FILE != 0;
        let is_non_dir_request = create_options & FILE_NON_DIRECTORY_FILE != 0;
        let delete_on_close = create_options & FILE_DELETE_ON_CLOSE != 0;

        let mut inner = self.inner.lock().unwrap();

        if path.is_empty() {
            log_op!("create_file: root directory");
            return Ok(CreateFileInfo {
                context: HandleContext {
                    path: String::new(),
                    delete_on_close: false,
                },
                is_dir: true,
                new_file_created: false,
            });
        }

        let p = parent_path(&path).to_owned();
        if !p.is_empty() && !inner.nodes.contains_key(&p) {
            return Err(STATUS_OBJECT_PATH_NOT_FOUND);
        }

        if let Some(node) = inner.nodes.get(&path) {
            if is_dir_request && node.kind != EntryKind::Directory {
                return Err(STATUS_NOT_A_DIRECTORY);
            }
            if is_non_dir_request && node.kind == EntryKind::Directory {
                return Err(STATUS_FILE_IS_A_DIRECTORY);
            }

            if create_disposition == FILE_SUPERSEDE {
                if node.kind == EntryKind::Directory {
                    return Err(STATUS_ACCESS_DENIED);
                }
                if let Some(n) = inner.nodes.get_mut(&path) {
                    n.size = 0;
                    let now = Timestamp::now();
                    n.mtime = now;
                    n.ctime = now;
                    n.attrs = file_attributes;
                }
                inner.write_cache.insert(path.clone(), Vec::new());
                Ok(CreateFileInfo {
                    context: HandleContext {
                        path,
                        delete_on_close,
                    },
                    is_dir: false,
                    new_file_created: false,
                })
            } else if create_disposition == FILE_OVERWRITE || create_disposition == FILE_OVERWRITE_IF {
                if node.kind == EntryKind::Directory {
                    return Err(STATUS_ACCESS_DENIED);
                }
                if let Some(n) = inner.nodes.get_mut(&path) {
                    n.size = 0;
                    let now = Timestamp::now();
                    n.mtime = now;
                    n.ctime = now;
                    n.attrs = file_attributes;
                }
                inner.write_cache.insert(path.clone(), Vec::new());
                Ok(CreateFileInfo {
                    context: HandleContext {
                        path,
                        delete_on_close,
                    },
                    is_dir: false,
                    new_file_created: false,
                })
            } else if create_disposition == FILE_CREATE {
                Err(STATUS_OBJECT_NAME_COLLISION)
            } else if create_disposition == FILE_OPEN || create_disposition == FILE_OPEN_IF {
                Ok(CreateFileInfo {
                    context: HandleContext {
                        path,
                        delete_on_close,
                    },
                    is_dir: node.kind == EntryKind::Directory,
                    new_file_created: false,
                })
            } else {
                Err(STATUS_INVALID_PARAMETER)
            }
        } else {
            if create_disposition == FILE_CREATE
                || create_disposition == FILE_OPEN_IF
                || create_disposition == FILE_OVERWRITE_IF
                || create_disposition == FILE_SUPERSEDE
            {
                let kind = if is_dir_request {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                let now = Timestamp::now();
                let mode = win_attrs_to_mode(file_attributes, kind);
                let attrs = mode_to_win_attrs(mode, kind);

                inner.nodes.insert(
                    path.clone(),
                    FsNode {
                        kind,
                        size: 0,
                        mode,
                        attrs,
                        atime: now,
                        mtime: now,
                        ctime: now,
                    },
                );
                inner.children.entry(path.clone()).or_default();
                inner
					.children
					.entry(p)
					.or_default()
					.push(path.clone());

                if kind == EntryKind::Directory {
                    let _ = inner.archive.upsert_entry(
                        crate::storage::IndexEntry::new_directory(path.clone(), mode, 0, 0),
                        None,
                    );
                } else {
                    let _ = inner.archive.upsert_entry(
                        crate::storage::IndexEntry::new_file(path.clone(), mode, 0, 0),
                        Some(&[]),
                    );
                }
                inner.mark_mutated();

                Ok(CreateFileInfo {
                    context: HandleContext {
                        path,
                        delete_on_close,
                    },
                    is_dir: kind == EntryKind::Directory,
                    new_file_created: true,
                })
            } else if create_disposition == FILE_OPEN || create_disposition == FILE_OVERWRITE {
                Err(STATUS_OBJECT_NAME_NOT_FOUND)
            } else {
                Err(STATUS_INVALID_PARAMETER)
            }
        }
    }

    fn close_file(
        &'h self,
        _file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.commit_write(&context.path);
    }

    fn cleanup(
        &'h self,
        _file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) {
        if !context.delete_on_close {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.write_cache.remove(&context.path);
        let path = &context.path;

        if let Some(node) = inner.nodes.get(path) {
            if node.kind == EntryKind::Directory {
                if let Some(children) = inner.children.get(path) {
                    if !children.is_empty() {
                        return;
                    }
                }
            }
        }

        inner.nodes.remove(path);
        inner.children.remove(path);
        let p = parent_path(path).to_owned();
        if let Some(children) = inner.children.get_mut(&p) {
            children.retain(|c| c != path);
        }

        if inner.archive.remove_prefix(path).is_ok() {
            inner.mark_mutated();
            let _ = inner.archive.sync();
        }
    }

    fn read_file(
        &'h self,
        _file_name: &U16CStr,
        offset: i64,
        buffer: &mut [u8],
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<u32> {
        if offset < 0 {
            return Err(STATUS_INVALID_PARAMETER);
        }

        let mut inner = self.inner.lock().unwrap();
        let path = &context.path;

        if let Some(cached) = inner.write_cache.get(path) {
            let start = (offset as usize).min(cached.len());
            let end = start.saturating_add(buffer.len()).min(cached.len());
            let bytes = end - start;
            buffer[..bytes].copy_from_slice(&cached[start..end]);
            return Ok(bytes as u32);
        }

        let node = inner.nodes.get(path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;
        if node.kind == EntryKind::Directory {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }

        let data = inner
            .archive
            .read_file(path)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;

        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(buffer.len()).min(data.len());
        let bytes = end - start;
        buffer[..bytes].copy_from_slice(&data[start..end]);
        Ok(bytes as u32)
    }

    fn write_file(
		&'h self,
		_file_name: &U16CStr,
		offset: i64,
		buffer: &[u8],
		info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<u32> {
		if offset < 0 {
			return Err(STATUS_INVALID_PARAMETER);
		}

		let mut inner = self.inner.lock().unwrap();
		let path = &context.path;

		{
			let node = inner
				.nodes
				.get(path)
				.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

			if node.kind != EntryKind::File {
				return Err(STATUS_ACCESS_DENIED);
			}
		}

		let new_size = {
			let data = inner.ensure_write_cache(path)?;

			let start = if info.write_to_eof() {
				data.len()
			} else {
				usize::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?
			};

			let end = start
				.checked_add(buffer.len())
				.ok_or(STATUS_INVALID_PARAMETER)?;

			if end > data.len() {
				data.resize(end, 0);
			}

			data[start..end].copy_from_slice(buffer);
			data.len() as u64
		};

		if let Some(node) = inner.nodes.get_mut(path) {
			node.size = new_size;
			let now = Timestamp::now();
			node.mtime = now;
			node.ctime = now;
		}

		Ok(buffer.len() as u32)
	}

    fn flush_file_buffers(
        &'h self,
        _file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.commit_write(&context.path)
    }

    fn get_file_information(
        &'h self,
        _file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<FileInfo> {
        let inner = self.inner.lock().unwrap();
        let node = inner.nodes.get(&context.path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

        let size = if let Some(cached) = inner.write_cache.get(&context.path) {
            cached.len() as u64
        } else {
            node.size
        };

        let is_dir = node.kind == EntryKind::Directory;
        let attrs = if is_dir {
            node.attrs | winnt::FILE_ATTRIBUTE_DIRECTORY
        } else {
            node.attrs
        };

        Ok(FileInfo {
            attributes: if attrs == winnt::FILE_ATTRIBUTE_DIRECTORY {
                attrs
            } else if attrs == 0 {
                winnt::FILE_ATTRIBUTE_NORMAL
            } else {
                attrs
            },
            creation_time: timestamp_to_system_time(node.ctime),
            last_access_time: timestamp_to_system_time(node.atime),
            last_write_time: timestamp_to_system_time(node.mtime),
            file_size: size,
            number_of_links: 1,
            file_index: 0,
        })
    }

    fn find_files(
        &'h self,
        _file_name: &U16CStr,
        mut fill_find_data: impl FnMut(&FindData) -> FillDataResult,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let inner = self.inner.lock().unwrap();

        let node = inner.nodes.get(&context.path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;
        if node.kind != EntryKind::Directory {
            return Err(STATUS_INVALID_DEVICE_REQUEST);
        }

        let children = inner
            .children
            .get(&context.path)
            .cloned()
            .unwrap_or_default();

        for child_path in &children {
            if let Some(child_node) = inner.nodes.get(child_path) {
                let name_wide = U16CString::from_str(file_name_from_path(child_path))
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;

                let attrs = if child_node.kind == EntryKind::Directory {
                    child_node.attrs | winnt::FILE_ATTRIBUTE_DIRECTORY
                } else if child_node.attrs == 0 {
                    winnt::FILE_ATTRIBUTE_NORMAL
                } else {
                    child_node.attrs
                };

                let size = if let Some(cached) = inner.write_cache.get(child_path) {
                    cached.len() as u64
                } else {
                    child_node.size
                };

                let result = fill_find_data(&FindData {
                    attributes: attrs,
                    creation_time: timestamp_to_system_time(child_node.ctime),
                    last_access_time: timestamp_to_system_time(child_node.atime),
                    last_write_time: timestamp_to_system_time(child_node.mtime),
                    file_size: size,
                    file_name: name_wide,
                });

                match result {
                    Ok(()) => {}
                    Err(dokan::FillDataError::NameTooLong) => {}
                    Err(dokan::FillDataError::BufferFull) => break,
                }
            }
        }

        Ok(())
    }

    fn set_file_attributes(
        &'h self,
        _file_name: &U16CStr,
        file_attributes: u32,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let mut inner = self.inner.lock().unwrap();
        let node = inner.nodes.get_mut(&context.path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

        node.attrs = file_attributes;
        node.mode = win_attrs_to_mode(file_attributes, node.kind);
        let now = Timestamp::now();
        node.atime = now;
        node.ctime = now;

        let entry = crate::storage::IndexEntry {
            virtual_path: context.path.clone(),
            kind: node.kind,
            start_byte: 0,
            block_length: node.size,
            mode: node.mode,
            uid: 0,
            gid: 0,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
        };

        inner
            .archive
            .upsert_entry(entry, None)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        inner.mark_mutated();
        Ok(())
    }

    fn set_file_time(
        &'h self,
        _file_name: &U16CStr,
        creation_time: FileTimeOperation,
        last_access_time: FileTimeOperation,
        last_write_time: FileTimeOperation,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let mut inner = self.inner.lock().unwrap();
        let node = inner.nodes.get_mut(&context.path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

        if let FileTimeOperation::SetTime(t) = creation_time {
            node.ctime = Timestamp::from_system_time(t);
        }
        if let FileTimeOperation::SetTime(t) = last_access_time {
            node.atime = Timestamp::from_system_time(t);
        }
        if let FileTimeOperation::SetTime(t) = last_write_time {
            node.mtime = Timestamp::from_system_time(t);
        }

        let entry = crate::storage::IndexEntry {
            virtual_path: context.path.clone(),
            kind: node.kind,
            start_byte: 0,
            block_length: node.size,
            mode: node.mode,
            uid: 0,
            gid: 0,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
        };

        inner
            .archive
            .upsert_entry(entry, None)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        inner.mark_mutated();
        Ok(())
    }

    fn delete_file(
        &'h self,
        _file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let inner = self.inner.lock().unwrap();
        let node = inner.nodes.get(&context.path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

        if node.kind == EntryKind::Directory {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }

        if node.attrs & winnt::FILE_ATTRIBUTE_READONLY != 0 {
            return Err(STATUS_ACCESS_DENIED);
        }

        Ok(())
    }

    fn delete_directory(
        &'h self,
        _file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let inner = self.inner.lock().unwrap();
        let node = inner.nodes.get(&context.path).ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

        if node.kind != EntryKind::Directory {
            return Err(STATUS_NOT_A_DIRECTORY);
        }

        if context.path.is_empty() {
            return Err(STATUS_ACCESS_DENIED);
        }

        let children = inner.children.get(&context.path);
        if children.map(|c| !c.is_empty()).unwrap_or(false) {
            return Err(STATUS_DIRECTORY_NOT_EMPTY);
        }

        Ok(())
    }

    fn move_file(
        &'h self,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_existing: bool,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        let old_path = normalize_pons_path(file_name)?;
        let new_path = normalize_pons_path(new_file_name)?;

        if old_path.is_empty() {
            return Err(STATUS_ACCESS_DENIED);
        }

        let mut inner = self.inner.lock().unwrap();

        if !inner.nodes.contains_key(&old_path) {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }

        let new_parent = parent_path(&new_path).to_owned();
        if !new_parent.is_empty() && !inner.nodes.contains_key(&new_parent) {
            return Err(STATUS_OBJECT_PATH_NOT_FOUND);
        }

        let old_prefix = format!("{}/", old_path);
        if new_path == old_path || new_path.starts_with(&old_prefix) {
            return Err(STATUS_INVALID_PARAMETER);
        }

        let old_kind = inner.nodes.get(&old_path).map(|n| n.kind).unwrap();

        let remove_tree = |inner: &mut Inner, path: &str| {
            let prefix = format!("{}/", path);
            let to_remove: Vec<String> = inner
                .nodes
                .keys()
                .filter(|p| *p == path || p.starts_with(&prefix))
                .cloned()
                .collect();
            for p in &to_remove {
                inner.nodes.remove(p);
                inner.children.remove(p);
                let pp = parent_path(p).to_owned();
                if let Some(c) = inner.children.get_mut(&pp) {
                    c.retain(|c| c != p);
                }
            }
        };

        if inner.nodes.contains_key(&new_path) {
            if !replace_if_existing {
                return Err(STATUS_OBJECT_NAME_COLLISION);
            }
            let existing_kind = inner.nodes.get(&new_path).map(|n| n.kind).unwrap();

            if old_kind == EntryKind::Directory && existing_kind != EntryKind::Directory {
                return Err(STATUS_NOT_A_DIRECTORY);
            }
            if old_kind != EntryKind::Directory && existing_kind == EntryKind::Directory {
                return Err(STATUS_FILE_IS_A_DIRECTORY);
            }
            if existing_kind == EntryKind::Directory {
                let children = inner.children.get(&new_path);
                if children.map(|c| !c.is_empty()).unwrap_or(false) {
                    return Err(STATUS_DIRECTORY_NOT_EMPTY);
                }
            }

            remove_tree(&mut inner, &new_path);
        }

        inner
            .archive
            .rename_prefix(&old_path, &new_path)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        inner.mark_mutated();

		// Move pending, not-yet-committed write buffers along with the renamed paths.
		let cached_paths: Vec<String> = inner
			.write_cache
			.keys()
			.filter(|path| *path == &old_path || path.starts_with(&old_prefix))
			.cloned()
			.collect();

		for old_cached_path in cached_paths {
			let Some(data) = inner.write_cache.remove(&old_cached_path) else {
				continue;
			};

			let suffix = if old_cached_path == old_path {
				""
			} else {
				old_cached_path
					.strip_prefix(&old_prefix)
					.unwrap_or_default()
			};

			let target = if suffix.is_empty() {
				new_path.clone()
			} else {
				format!("{new_path}/{}", suffix.trim_start_matches('/'))
			};

			inner.write_cache.insert(target, data);
		}

        let to_move: Vec<(String, FsNode)> = inner
            .nodes
            .iter()
            .filter(|(p, _)| *p == &old_path || p.starts_with(&old_prefix))
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect();

        for (p, _) in &to_move {
            inner.nodes.remove(p);
            inner.children.remove(p);
            let pp = parent_path(p).to_owned();
            if let Some(c) = inner.children.get_mut(&pp) {
                c.retain(|c| c != p);
            }
        }

        for (old, node) in to_move {
            let suffix = if old == old_path {
                String::new()
            } else {
                old.strip_prefix(&old_prefix)
                    .unwrap_or("")
                    .to_owned()
            };

            let target = if suffix.is_empty() {
                new_path.clone()
            } else {
                format!("{}/{}", new_path, suffix.trim_start_matches('/'))
            };

            inner.nodes.insert(target.clone(), node);
            let tp = parent_path(&target).to_owned();
            inner.children.entry(tp).or_default().push(target);
        }

        Ok(())
    }

    fn set_end_of_file(
		&'h self,
		_file_name: &U16CStr,
		offset: i64,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if offset < 0 {
			return Err(STATUS_INVALID_PARAMETER);
		}

		let new_len = usize::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?;

		let mut inner = self.inner.lock().unwrap();
		let path = &context.path;

		{
			let node = inner
				.nodes
				.get(path)
				.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

			if node.kind != EntryKind::File {
				return Err(STATUS_INVALID_DEVICE_REQUEST);
			}
		}

		{
			let data = inner.ensure_write_cache(path)?;
			data.resize(new_len, 0);
		}

		if let Some(node) = inner.nodes.get_mut(path) {
			node.size = new_len as u64;
			let now = Timestamp::now();
			node.mtime = now;
			node.ctime = now;
		}

		Ok(())
	}

	fn set_allocation_size(
		&'h self,
		_file_name: &U16CStr,
		alloc_size: i64,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if alloc_size < 0 {
			return Err(STATUS_INVALID_PARAMETER);
		}

		let requested = usize::try_from(alloc_size).map_err(|_| STATUS_INVALID_PARAMETER)?;

		let mut inner = self.inner.lock().unwrap();
		let path = &context.path;

		{
			let node = inner
				.nodes
				.get(path)
				.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;

			if node.kind != EntryKind::File {
				return Err(STATUS_INVALID_DEVICE_REQUEST);
			}
		}

		let logical_size = {
			let data = inner.ensure_write_cache(path)?;

			// Windows allocation size is capacity, not necessarily logical EOF.
			// Shrinking allocation below EOF truncates the file; growing it only
			// reserves memory.
			if requested < data.len() {
				data.truncate(requested);
			} else {
				data.reserve(requested.saturating_sub(data.capacity()));
			}

			data.len() as u64
		};

		if let Some(node) = inner.nodes.get_mut(path) {
			node.size = logical_size;
			let now = Timestamp::now();
			node.mtime = now;
			node.ctime = now;
		}

		Ok(())
	}

    fn get_disk_free_space(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<DiskSpaceInfo> {
        let inner = self.inner.lock().unwrap();
        let used_bytes: u64 = inner.nodes.values().map(|n| n.size).sum();
        let total = used_bytes.saturating_add(1 << 30);

        Ok(DiskSpaceInfo {
            byte_count: total,
            free_byte_count: total.saturating_sub(used_bytes),
            available_byte_count: total.saturating_sub(used_bytes),
        })
    }

    fn get_volume_information(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<VolumeInfo> {
        let name = U16CString::from_str(&self.volume_name).map_err(|_| STATUS_INTERNAL_ERROR)?;
        let fs_name = U16CString::from_str("MOSS").map_err(|_| STATUS_INTERNAL_ERROR)?;

        Ok(VolumeInfo {
            name,
            serial_number: 0,
            max_component_length: 255,
            fs_flags: winnt::FILE_CASE_PRESERVED_NAMES
                | winnt::FILE_UNICODE_ON_DISK
                | winnt::FILE_FILE_COMPRESSION,
            fs_name,
        })
    }

    fn mounted(
        &'h self,
        mount_point: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<()> {
        log_op!("mounted: {:?}", mount_point);
        Ok(())
    }

    fn unmounted(&'h self, _info: &OperationInfo<'c, 'h, Self>) -> OperationResult<()> {
        log_op!("unmounted called");

        let mut inner = self.inner.lock().unwrap();

        let pending: Vec<String> = inner.write_cache.keys().cloned().collect();
        for path in pending {
            let _ = inner.commit_write(&path);
        }

        let _ = inner.force_sync();

        log_op!("unmounted complete");
        Ok(())
    }

    fn get_file_security(
        &'h self,
        _file_name: &U16CStr,
        _security_information: u32,
        _security_descriptor: winnt::PSECURITY_DESCRIPTOR,
        _buffer_length: u32,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<u32> {
        Err(STATUS_NOT_IMPLEMENTED)
    }

    fn set_file_security(
        &'h self,
        _file_name: &U16CStr,
        _security_information: u32,
        _security_descriptor: winnt::PSECURITY_DESCRIPTOR,
        _buffer_length: u32,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        Err(STATUS_NOT_IMPLEMENTED)
    }

    fn find_streams(
        &'h self,
        file_name: &U16CStr,
        fill_find_stream_data: impl FnMut(&dokan::FindStreamData) -> FillDataResult,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        log_op!("find_streams: {:?}", file_name);
        // No alternate data streams; just return success.
        let _ = fill_find_stream_data;
        Ok(())
    }
}

pub fn mount(archive: Moss, mount_point: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mount_point_wide = U16CString::from_str(mount_point)?;
    let vol_name = archive.path()
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "moss".to_owned());
    let handler = MossDokan::new(archive, vol_name);

    let options = MountOptions {
        flags: MountFlags::MOUNT_MANAGER
            | MountFlags::CURRENT_SESSION
            | MountFlags::DEBUG
            | MountFlags::STDERR,
        timeout: Duration::from_secs(30),
        ..Default::default()
    };

    setup_panic_hook();
    log_msg("[Moss::dokan] init");
    dokan::init();

    let mut mounter = FileSystemMounter::new(&handler, &mount_point_wide, &options);
    log_op!("[Moss::dokan] calling DokanCreateFileSystem...");
    let file_system = mounter.mount().map_err(|e| {
        log_op!("[Moss::dokan] mount failed: {:?}", e);
        e
    })?;
    log_op!("[Moss::dokan] mount succeeded, waiting for unmount...");

    drop(file_system);
    log_op!("[Moss::dokan] filesystem closed");
    dokan::shutdown();
    log_op!("[Moss::dokan] shutdown complete");
    Ok(())
}


