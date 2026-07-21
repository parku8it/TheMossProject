use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime},
};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use libc::{EACCES, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSYS, ENOTDIR, ENOTEMPTY, EOPNOTSUPP};

use crate::storage::{
    EntryKind, IdleCheckpoint, IndexEntry, Moss, SharedMoss, Timestamp,
};

// FUSE kernel timeout is ~1s — batch index writes to avoid EIO
const ROOT_INODE: u64 = 1;
const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4096;
const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;
const IDLE_CHECKPOINT_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct FsNode {
    ino: u64,
    name: OsString,
    parent: u64,
    kind: EntryKind,
    mode: u16,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Timestamp,
    mtime: Timestamp,
    ctime: Timestamp,
}

pub struct MossFS {
    archive: SharedMoss,
    idle_checkpoint: Arc<IdleCheckpoint>,
    nodes: HashMap<u64, FsNode>,
    children: HashMap<u64, Vec<u64>>,
    write_cache: HashMap<u64, Vec<u8>>,
    next_inode: u64,
    next_handle: u64,
}

impl MossFS {
    pub fn new(archive: Moss) -> Self {
		let now = Timestamp::now();
		let archive = Arc::new(Mutex::new(archive));
		let idle_checkpoint =
			IdleCheckpoint::start(&archive, IDLE_CHECKPOINT_DELAY);

		let mut filesystem = Self {
			archive,
			idle_checkpoint,
			nodes: HashMap::new(),
			children: HashMap::new(),
			write_cache: HashMap::new(),
			next_inode: ROOT_INODE + 1,
			next_handle: 1,
		};

		filesystem.nodes.insert(
			ROOT_INODE,
			FsNode {
				ino: ROOT_INODE,
				name: OsString::new(),
				parent: ROOT_INODE,
				kind: EntryKind::Directory,
				mode: 0o755,
				uid: current_uid(),
				gid: current_gid(),
				size: 0,
				atime: now,
				mtime: now,
				ctime: now,
			},
		);

		filesystem.children.insert(ROOT_INODE, Vec::new());
		filesystem.build_tree();
		filesystem
	}

	fn archive_lock(&self) -> MutexGuard<'_, Moss> {
		match self.archive.lock() {
			Ok(archive) => archive,
			Err(poisoned) => poisoned.into_inner(),
		}
	}

	fn sync_all_pending(&mut self) {
		let pending: Vec<u64> = self.write_cache.keys().copied().collect();

		for inode in pending {
			let _ = self.commit_inode(inode);
		}

		let _ = self.idle_checkpoint.force(&self.archive);
	}

    fn build_tree(&mut self) {
        let entries = {
			let archive = self.archive_lock();
			archive.entries().cloned().collect::<Vec<_>>()
		};

        let mut path_to_inode = HashMap::<String, u64>::new();
        path_to_inode.insert(String::new(), ROOT_INODE);

        for entry in entries {
            let parts = entry.virtual_path.split('/').collect::<Vec<_>>();
            let mut current_path = String::new();
            let mut parent = ROOT_INODE;

            for (position, part) in parts.iter().enumerate() {
                if !current_path.is_empty() {
                    current_path.push('/');
                }
                current_path.push_str(part);

                let is_last = position + 1 == parts.len();

                if let Some(&inode) = path_to_inode.get(&current_path) {
                    if is_last {
                        if let Some(node) = self.nodes.get_mut(&inode) {
                            node.kind = entry.kind;
                            node.mode = entry.mode;
                            node.uid = entry.uid;
                            node.gid = entry.gid;
                            node.size = entry.block_length;
                            node.atime = entry.atime;
                            node.mtime = entry.mtime;
                            node.ctime = entry.ctime;
                        }
                    }

                    parent = inode;
                    continue;
                }

                let inode = self.allocate_inode();
                let kind = if is_last {
                    entry.kind
                } else {
                    EntryKind::Directory
                };

                let node = FsNode {
                    ino: inode,
                    name: OsString::from(part),
                    parent,
                    kind,
                    mode: if is_last { entry.mode } else { 0o755 },
                    uid: if is_last { entry.uid } else { current_uid() },
                    gid: if is_last { entry.gid } else { current_gid() },
                    size: if is_last && kind != EntryKind::Directory {
                        entry.block_length
                    } else {
                        0
                    },
                    atime: entry.atime,
                    mtime: entry.mtime,
                    ctime: entry.ctime,
                };

                self.nodes.insert(inode, node);
                self.children.entry(parent).or_default().push(inode);
                self.children.entry(inode).or_default();
                path_to_inode.insert(current_path.clone(), inode);
                parent = inode;
            }
        }

        self.sort_all_children();
    }

    fn allocate_inode(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode = self.next_inode.saturating_add(1);
        inode
    }

    fn allocate_handle(&mut self) -> u64 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.saturating_add(1);
        handle
    }

    fn sort_all_children(&mut self) {
        let nodes = &self.nodes;

        for children in self.children.values_mut() {
            children.sort_unstable_by(|left, right| {
                let left_name = nodes
                    .get(left)
                    .map(|node| node.name.as_os_str())
                    .unwrap_or_default();

                let right_name = nodes
                    .get(right)
                    .map(|node| node.name.as_os_str())
                    .unwrap_or_default();

                left_name.cmp(right_name)
            });
        }
    }

	fn insert_child_sorted(&mut self, parent: u64, inode: u64) {
		let Some(name) = self.nodes.get(&inode).map(|node| node.name.clone()) else {
			return;
		};

		let nodes = &self.nodes;
		let children = self.children.entry(parent).or_default();

		let position = children
			.binary_search_by(|candidate| {
				nodes
					.get(candidate)
					.map(|node| node.name.as_os_str())
					.unwrap_or_default()
					.cmp(name.as_os_str())
			})
			.unwrap_or_else(|position| position);

		children.insert(position, inode);
	}

    fn find_child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.children.get(&parent)?.iter().copied().find(|inode| {
            self.nodes
                .get(inode)
                .map(|node| node.name == name)
                .unwrap_or(false)
        })
    }

    fn validate_parent(&self, parent: u64) -> Result<(), i32> {
        match self.nodes.get(&parent) {
            Some(node) if node.kind == EntryKind::Directory => Ok(()),
            Some(_) => Err(ENOTDIR),
            None => Err(ENOENT),
        }
    }

    fn validate_name(name: &OsStr) -> Result<String, i32> {
        let name = name.to_str().ok_or(EINVAL)?;

        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
            || name.as_bytes().contains(&0)
        {
            return Err(EINVAL);
        }

        Ok(name.to_owned())
    }

    fn path_for_inode(&self, inode: u64) -> Result<String, i32> {
        if inode == ROOT_INODE {
            return Ok(String::new());
        }

        let mut current = inode;
        let mut components = Vec::new();
        let mut iterations = 0_usize;

        while current != ROOT_INODE {
            iterations += 1;

            if iterations > self.nodes.len() {
                return Err(EIO);
            }

            let node = self.nodes.get(&current).ok_or(ENOENT)?;
            let component = node.name.to_str().ok_or(EINVAL)?;
            components.push(component.to_owned());
            current = node.parent;
        }

        components.reverse();
        Ok(components.join("/"))
    }

    fn file_type(kind: EntryKind) -> FileType {
        match kind {
            EntryKind::File => FileType::RegularFile,
            EntryKind::Directory => FileType::Directory,
            EntryKind::Symlink => FileType::Symlink,
        }
    }

    fn attr_for_inode(&self, inode: u64) -> Option<FileAttr> {
        let node = self.nodes.get(&inode)?;

        let directory_links = if node.kind == EntryKind::Directory {
            self.children
                .get(&inode)
                .map(|children| {
                    children
                        .iter()
                        .filter(|child| {
                            self.nodes
                                .get(child)
                                .map(|node| node.kind == EntryKind::Directory)
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };

        let nlink = if node.kind == EntryKind::Directory {
            2_u32.saturating_add(directory_links.min(u32::MAX as usize) as u32)
        } else {
            1
        };

        Some(FileAttr {
            ino: node.ino,
            size: node.size,
            blocks: node.size.saturating_add(511) / 512,
            atime: node.atime.to_system_time(),
            mtime: node.mtime.to_system_time(),
            ctime: node.ctime.to_system_time(),
            crtime: node.ctime.to_system_time(),
            kind: Self::file_type(node.kind),
            perm: node.mode,
            nlink,
            uid: node.uid,
            gid: node.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        })
    }

    fn node_as_entry(&self, inode: u64) -> Result<IndexEntry, i32> {
        let node = self.nodes.get(&inode).ok_or(ENOENT)?;
        let path = self.path_for_inode(inode)?;

        Ok(IndexEntry {
            virtual_path: path,
            kind: node.kind,
            start_byte: 0,
            block_length: node.size,
            mode: node.mode,
            uid: node.uid,
            gid: node.gid,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
        })
    }

    fn persist_metadata(&mut self, inode: u64) -> Result<(), i32> {
		if inode == ROOT_INODE {
			return Ok(());
		}

		let entry = self.node_as_entry(inode)?;

		let result = self
			.archive_lock()
			.upsert_entry(entry, None)
			.map_err(io_error_to_errno);

		if result.is_ok() {
			self.idle_checkpoint.mark_mutated();
		}

		result
	}

    fn create_node(
        &mut self,
        parent: u64,
        name: &OsStr,
        kind: EntryKind,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64, i32> {
        self.validate_parent(parent)?;
        let name = Self::validate_name(name)?;

        if self.find_child(parent, OsStr::new(&name)).is_some() {
            return Err(EEXIST);
        }

        let inode = self.allocate_inode();
        let now = Timestamp::now();

        let node = FsNode {
            ino: inode,
            name: OsString::from(&name),
            parent,
            kind,
            mode: (mode & 0o7777) as u16,
            uid,
            gid,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
        };

        self.nodes.insert(inode, node);
		self.children.entry(inode).or_default();
		self.insert_child_sorted(parent, inode);

        let result = match kind {
			EntryKind::Directory => self.persist_metadata(inode),
			EntryKind::File | EntryKind::Symlink => {
				let entry = self.node_as_entry(inode)?;

				let result = self
					.archive_lock()
					.upsert_entry(entry, Some(&[]))
					.map_err(io_error_to_errno);

				if result.is_ok() {
					self.idle_checkpoint.mark_mutated();
				}

				result
			}
		};

        if let Err(error) = result {
            self.nodes.remove(&inode);
            self.children.remove(&inode);

            if let Some(children) = self.children.get_mut(&parent) {
                children.retain(|child| *child != inode);
            }

            return Err(error);
        }

        self.touch_directory(parent);
        Ok(inode)
    }

    fn touch_directory(&mut self, inode: u64) {
        if let Some(node) = self.nodes.get_mut(&inode) {
            let now = Timestamp::now();
            node.mtime = now;
            node.ctime = now;
        }

        if inode != ROOT_INODE {
            let _ = self.persist_metadata(inode);
        }
    }

    fn load_file_data(&mut self, inode: u64) -> Result<Vec<u8>, i32> {
        let node = self.nodes.get(&inode).ok_or(ENOENT)?;

        if node.kind == EntryKind::Directory {
            return Err(EISDIR);
        }

        let path = self.path_for_inode(inode)?;

        self.archive_lock()
			.read_file(&path)
			.map_err(io_error_to_errno)
    }

    fn ensure_write_cache(&mut self, inode: u64) -> Result<&mut Vec<u8>, i32> {
        if !self.write_cache.contains_key(&inode) {
            let data = self.load_file_data(inode)?;
            self.write_cache.insert(inode, data);
        }

        self.write_cache.get_mut(&inode).ok_or(EIO)
    }

    fn commit_inode(&mut self, inode: u64) -> Result<(), i32> {
		let Some(data) = self.write_cache.remove(&inode) else {
			return Ok(());
		};

		let mut entry = self.node_as_entry(inode)?;
		let now = Timestamp::now();

		entry.block_length = data.len() as u64;
		entry.mtime = now;
		entry.ctime = now;

		let write_result = self
			.archive_lock()
			.upsert_entry(entry, Some(&data));

		if let Err(error) = write_result {
			self.write_cache.insert(inode, data);
			return Err(io_error_to_errno(error));
		}

		self.idle_checkpoint.mark_mutated();

		if let Some(node) = self.nodes.get_mut(&inode) {
			node.size = data.len() as u64;
			node.mtime = now;
			node.ctime = now;
		}

		Ok(())
	}

    fn remove_node(&mut self, parent: u64, inode: u64) -> Result<(), i32> {
        let path = self.path_for_inode(inode)?;

        self.archive_lock()
			.remove_prefix(&path)
			.map_err(io_error_to_errno)?;

		self.idle_checkpoint.mark_mutated();

        self.write_cache.remove(&inode);
        self.nodes.remove(&inode);
        self.children.remove(&inode);

        if let Some(children) = self.children.get_mut(&parent) {
            children.retain(|child| *child != inode);
        }

        self.touch_directory(parent);
        Ok(())
    }

    fn set_file_size(&mut self, inode: u64, size: u64) -> Result<(), i32> {
        let size = usize::try_from(size).map_err(|_| EINVAL)?;
        let cache = self.ensure_write_cache(inode)?;
        cache.resize(size, 0);

        if let Some(node) = self.nodes.get_mut(&inode) {
            node.size = size as u64;
            let now = Timestamp::now();
            node.mtime = now;
            node.ctime = now;
        }

        Ok(())
    }

    fn read_data(&mut self, inode: u64) -> Result<Vec<u8>, i32> {
        if let Some(data) = self.write_cache.get(&inode) {
            return Ok(data.clone());
        }

        self.load_file_data(inode)
    }
}

impl Filesystem for MossFS {
    fn lookup(&mut self, _request: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if let Err(error) = self.validate_parent(parent) {
            reply.error(error);
            return;
        }

        match self.find_child(parent, name) {
            Some(inode) => match self.attr_for_inode(inode) {
                Some(attribute) => reply.entry(&TTL, &attribute, 0),
                None => reply.error(ENOENT),
            },
            None => reply.error(ENOENT),
        }
    }

    fn getattr(&mut self, _request: &Request, inode: u64, reply: ReplyAttr) {
        match self.attr_for_inode(inode) {
            Some(attribute) => reply.attr(&TTL, &attribute),
            None => reply.error(ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _request: &Request,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _file_handle: Option<u64>,
        _creation_time: Option<SystemTime>,
        _change_time: Option<SystemTime>,
        _backup_time: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if !self.nodes.contains_key(&inode) {
            reply.error(ENOENT);
            return;
        }

        if let Some(size) = size {
            let kind = self.nodes.get(&inode).map(|node| node.kind);

            if kind == Some(EntryKind::Directory) {
                reply.error(EISDIR);
                return;
            }

            if let Err(error) = self.set_file_size(inode, size) {
                reply.error(error);
                return;
            }
        }

        if let Some(node) = self.nodes.get_mut(&inode) {
            if let Some(mode) = mode {
                node.mode = (mode & 0o7777) as u16;
            }

            if let Some(uid) = uid {
                node.uid = uid;
            }

            if let Some(gid) = gid {
                node.gid = gid;
            }

            if let Some(atime) = atime {
                node.atime = timestamp_from_time_or_now(atime);
            }

            if let Some(mtime) = mtime {
                node.mtime = timestamp_from_time_or_now(mtime);
            }

            node.ctime = Timestamp::now();
        }

        let persist_result = if self.write_cache.contains_key(&inode) {
            self.commit_inode(inode)
        } else {
            self.persist_metadata(inode)
        };

        if let Err(error) = persist_result {
            reply.error(error);
            return;
        }

        match self.attr_for_inode(inode) {
            Some(attribute) => reply.attr(&TTL, &attribute),
            None => reply.error(ENOENT),
        }
    }

    fn readlink(&mut self, _request: &Request, inode: u64, reply: ReplyData) {
        match self.nodes.get(&inode) {
            Some(node) if node.kind == EntryKind::Symlink => {}
            Some(_) => {
                reply.error(EINVAL);
                return;
            }
            None => {
                reply.error(ENOENT);
                return;
            }
        }

        match self.read_data(inode) {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(error),
        }
    }

    fn mknod(
        &mut self,
        request: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        match self.create_node(
            parent,
            name,
            EntryKind::File,
            mode,
            request.uid(),
            request.gid(),
        ) {
            Ok(inode) => match self.attr_for_inode(inode) {
                Some(attribute) => reply.entry(&TTL, &attribute, 0),
                None => reply.error(EIO),
            },
            Err(error) => reply.error(error),
        }
    }

	fn destroy(&mut self) {
		self.sync_all_pending();
	}

    fn mkdir(
        &mut self,
        request: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.create_node(
            parent,
            name,
            EntryKind::Directory,
            mode,
            request.uid(),
            request.gid(),
        ) {
            Ok(inode) => match self.attr_for_inode(inode) {
                Some(attribute) => reply.entry(&TTL, &attribute, 0),
                None => reply.error(EIO),
            },
            Err(error) => reply.error(error),
        }
    }

    fn unlink(&mut self, _request: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if let Err(error) = self.validate_parent(parent) {
            reply.error(error);
            return;
        }

        let Some(inode) = self.find_child(parent, name) else {
            reply.error(ENOENT);
            return;
        };

        match self.nodes.get(&inode) {
            Some(node) if node.kind == EntryKind::Directory => {
                reply.error(EISDIR);
                return;
            }
            Some(_) => {}
            None => {
                reply.error(ENOENT);
                return;
            }
        }

        match self.remove_node(parent, inode) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn rmdir(&mut self, _request: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if let Err(error) = self.validate_parent(parent) {
            reply.error(error);
            return;
        }

        let Some(inode) = self.find_child(parent, name) else {
            reply.error(ENOENT);
            return;
        };

        match self.nodes.get(&inode) {
            Some(node) if node.kind != EntryKind::Directory => {
                reply.error(ENOTDIR);
                return;
            }
            Some(_) => {}
            None => {
                reply.error(ENOENT);
                return;
            }
        }

        if self
            .children
            .get(&inode)
            .map(|children| !children.is_empty())
            .unwrap_or(false)
        {
            reply.error(ENOTEMPTY);
            return;
        }

        match self.remove_node(parent, inode) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn symlink(
        &mut self,
        request: &Request,
        parent: u64,
        name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let target_bytes = target.as_os_str().as_encoded_bytes();

        let inode = match self.create_node(
            parent,
            name,
            EntryKind::Symlink,
            0o777,
            request.uid(),
            request.gid(),
        ) {
            Ok(inode) => inode,
            Err(error) => {
                reply.error(error);
                return;
            }
        };

        let mut entry = match self.node_as_entry(inode) {
            Ok(entry) => entry,
            Err(error) => {
                reply.error(error);
                return;
            }
        };

        entry.block_length = target_bytes.len() as u64;

        let symlink_result = self
			.archive_lock()
			.upsert_entry(entry, Some(target_bytes));

		if let Err(error) = symlink_result {
            let parent = self
                .nodes
                .get(&inode)
                .map(|node| node.parent)
                .unwrap_or(ROOT_INODE);

            self.nodes.remove(&inode);
            self.children.remove(&inode);

            if let Some(children) = self.children.get_mut(&parent) {
                children.retain(|child| *child != inode);
            }

            reply.error(io_error_to_errno(error));
            return;
        }

        if let Some(node) = self.nodes.get_mut(&inode) {
            node.size = target_bytes.len() as u64;
        }

        match self.attr_for_inode(inode) {
            Some(attribute) => reply.entry(&TTL, &attribute, 0),
            None => reply.error(EIO),
        }
    }

    fn rename(
        &mut self,
        _request: &Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0 {
            reply.error(EINVAL);
            return;
        }

        if flags & RENAME_EXCHANGE != 0 {
            reply.error(EOPNOTSUPP);
            return;
        }

        if let Err(error) = self.validate_parent(parent) {
            reply.error(error);
            return;
        }

        if let Err(error) = self.validate_parent(new_parent) {
            reply.error(error);
            return;
        }

        let new_name = match Self::validate_name(new_name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };

        let Some(inode) = self.find_child(parent, name) else {
            reply.error(ENOENT);
            return;
        };

        let old_path = match self.path_for_inode(inode) {
            Ok(path) => path,
            Err(error) => {
                reply.error(error);
                return;
            }
        };

        if self.nodes.get(&inode).map(|node| node.kind) == Some(EntryKind::Directory) {
            let mut ancestor = new_parent;

            loop {
                if ancestor == inode {
                    reply.error(EINVAL);
                    return;
                }

                if ancestor == ROOT_INODE {
                    break;
                }

                let Some(node) = self.nodes.get(&ancestor) else {
                    reply.error(ENOENT);
                    return;
                };

                ancestor = node.parent;
            }
        }

        let existing = self.find_child(new_parent, OsStr::new(&new_name));

        if existing == Some(inode) {
            reply.ok();
            return;
        }

        if existing.is_some() && flags & RENAME_NOREPLACE != 0 {
            reply.error(EEXIST);
            return;
        }

        if let Some(existing_inode) = existing {
            let source_kind = self.nodes.get(&inode).map(|node| node.kind);
            let target_kind = self.nodes.get(&existing_inode).map(|node| node.kind);

            match (source_kind, target_kind) {
                (Some(EntryKind::Directory), Some(kind)) if kind != EntryKind::Directory => {
                    reply.error(ENOTDIR);
                    return;
                }
                (Some(kind), Some(EntryKind::Directory)) if kind != EntryKind::Directory => {
                    reply.error(EISDIR);
                    return;
                }
                (Some(EntryKind::Directory), Some(EntryKind::Directory)) => {
                    if self
                        .children
                        .get(&existing_inode)
                        .map(|children| !children.is_empty())
                        .unwrap_or(false)
                    {
                        reply.error(ENOTEMPTY);
                        return;
                    }
                }
                _ => {}
            }

            if let Err(error) = self.remove_node(new_parent, existing_inode) {
                reply.error(error);
                return;
            }
        }

        let new_parent_path = match self.path_for_inode(new_parent) {
            Ok(path) => path,
            Err(error) => {
                reply.error(error);
                return;
            }
        };

        let new_path = if new_parent_path.is_empty() {
            new_name.clone()
        } else {
            format!("{new_parent_path}/{new_name}")
        };

        let rename_result = self
			.archive_lock()
			.rename_prefix(&old_path, &new_path);

		if let Err(error) = rename_result {
			reply.error(io_error_to_errno(error));
			return;
		}

		self.idle_checkpoint.mark_mutated();

        if let Some(children) = self.children.get_mut(&parent) {
			children.retain(|child| *child != inode);
		}

		if let Some(node) = self.nodes.get_mut(&inode) {
			node.parent = new_parent;
			node.name = OsString::from(new_name);
			node.ctime = Timestamp::now();
		}

		self.insert_child_sorted(new_parent, inode);
		self.touch_directory(parent);

        if new_parent != parent {
            self.touch_directory(new_parent);
        }

        reply.ok();
    }

    fn link(
        &mut self,
        _request: &Request,
        _inode: u64,
        _new_parent: u64,
        _new_name: &OsStr,
        reply: ReplyEntry,
    ) {

        reply.error(EOPNOTSUPP);
    }

    fn open(&mut self, _request: &Request, inode: u64, flags: i32, reply: ReplyOpen) {
        let Some(node) = self.nodes.get(&inode) else {
            reply.error(ENOENT);
            return;
        };

        if node.kind == EntryKind::Directory {
            reply.error(EISDIR);
            return;
        }

        if flags & libc::O_TRUNC != 0 {
            if let Err(error) = self.set_file_size(inode, 0) {
                reply.error(error);
                return;
            }
        }

        reply.opened(self.allocate_handle(), 0);
    }

    fn read(
        &mut self,
        _request: &Request,
        inode: u64,
        _file_handle: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(EINVAL);
            return;
        }

        match self.nodes.get(&inode) {
            Some(node) if node.kind == EntryKind::Directory => {
                reply.error(EISDIR);
                return;
            }
            Some(_) => {}
            None => {
                reply.error(ENOENT);
                return;
            }
        }

        match self.read_data(inode) {
            Ok(data) => {
                let start = (offset as usize).min(data.len());
                let end = start.saturating_add(size as usize).min(data.len());
                reply.data(&data[start..end]);
            }
            Err(error) => reply.error(error),
        }
    }

    fn write(
        &mut self,
        _request: &Request,
        inode: u64,
        _file_handle: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(EINVAL);
            return;
        }

        match self.nodes.get(&inode) {
            Some(node) if node.kind == EntryKind::Directory => {
                reply.error(EISDIR);
                return;
            }
            Some(node) if node.kind == EntryKind::Symlink => {
                reply.error(EINVAL);
                return;
            }
            Some(_) => {}
            None => {
                reply.error(ENOENT);
                return;
            }
        }

        let append = flags & libc::O_APPEND != 0;

        let requested_offset = match usize::try_from(offset) {
            Ok(offset) => offset,
            Err(_) => {
                reply.error(EINVAL);
                return;
            }
        };

        let new_size = {
            let cache = match self.ensure_write_cache(inode) {
                Ok(cache) => cache,
                Err(error) => {
                    reply.error(error);
                    return;
                }
            };

            let start = if append {
                cache.len()
            } else {
                requested_offset
            };

            let Some(end) = start.checked_add(data.len()) else {
                reply.error(EINVAL);
                return;
            };

            if end > cache.len() {
                cache.resize(end, 0);
            }

            cache[start..end].copy_from_slice(data);
            cache.len() as u64
        };

        if let Some(node) = self.nodes.get_mut(&inode) {
            node.size = new_size;

            let now = Timestamp::now();
            node.mtime = now;
            node.ctime = now;
        }

        reply.written(data.len() as u32);
    }

    fn flush(
        &mut self,
        _request: &Request,
        inode: u64,
        _file_handle: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.commit_inode(inode) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn release(
        &mut self,
        _request: &Request,
        inode: u64,
        _file_handle: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.commit_inode(inode) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn fsync(
		&mut self,
		_request: &Request,
		inode: u64,
		_file_handle: u64,
		_datasync: bool,
		reply: ReplyEmpty,
	) {
		if let Err(error) = self.commit_inode(inode) {
			reply.error(error);
			return;
		}

		match self.idle_checkpoint.force(&self.archive) {
			Ok(()) => reply.ok(),
			Err(error) => reply.error(io_error_to_errno(error)),
		}
	}

    fn readdir(
        &mut self,
        _request: &Request,
        inode: u64,
        _file_handle: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(EINVAL);
            return;
        }

        let Some(directory) = self.nodes.get(&inode) else {
            reply.error(ENOENT);
            return;
        };

        if directory.kind != EntryKind::Directory {
            reply.error(ENOTDIR);
            return;
        }

        let parent = directory.parent;
        let mut entries: Vec<(u64, FileType, OsString)> = vec![
            (inode, FileType::Directory, OsString::from(".")),
            (parent, FileType::Directory, OsString::from("..")),
        ];

        if let Some(children) = self.children.get(&inode) {
            for child_inode in children {
                if let Some(node) = self.nodes.get(child_inode) {
                    entries.push((*child_inode, Self::file_type(node.kind), node.name.clone()));
                }
            }
        }

        for (index, (entry_inode, kind, name)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            if reply.add(entry_inode, (index + 1) as i64, kind, name) {
                break;
            }
        }

        reply.ok();
    }

    fn releasedir(
        &mut self,
        _request: &Request,
        _inode: u64,
        _file_handle: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsyncdir(
        &mut self,
        _request: &Request,
        _inode: u64,
        _file_handle: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&mut self, _request: &Request, _inode: u64, reply: ReplyStatfs) {
        let file_count = self.nodes.len() as u64;
        let used_bytes = self
            .nodes
            .values()
            .map(|node| node.size)
            .fold(0_u64, u64::saturating_add);

        let used_blocks = used_bytes.saturating_add(BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;

        let total_blocks = used_blocks.saturating_add(1_u64 << 30);
        let free_blocks = total_blocks.saturating_sub(used_blocks);

        reply.statfs(
            total_blocks,
            free_blocks,
            free_blocks,
            file_count.saturating_add(1_u64 << 20),
            1_u64 << 20,
            BLOCK_SIZE,
            255,
            BLOCK_SIZE,
        );
    }

    fn create(
        &mut self,
        request: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let inode = match self.create_node(
            parent,
            name,
            EntryKind::File,
            mode,
            request.uid(),
            request.gid(),
        ) {
            Ok(inode) => inode,
            Err(error) => {
                reply.error(error);
                return;
            }
        };

        let Some(attribute) = self.attr_for_inode(inode) else {
            reply.error(EIO);
            return;
        };

        let handle = self.allocate_handle();
        reply.created(&TTL, &attribute, 0, handle, flags as u32);
    }

    fn access(&mut self, _request: &Request, inode: u64, _mask: i32, reply: ReplyEmpty) {
        if !self.nodes.contains_key(&inode) {
            reply.error(ENOENT);
            return;
        }
        reply.ok();
    }

    fn getxattr(
        &mut self,
        _request: &Request,
        _inode: u64,
        _name: &OsStr,
        _size: u32,
        reply: fuser::ReplyXattr,
    ) {
        reply.error(ENOSYS);
    }

    fn listxattr(&mut self, _request: &Request, _inode: u64, _size: u32, reply: fuser::ReplyXattr) {
        reply.error(ENOSYS);
    }

    fn setxattr(
        &mut self,
        _request: &Request,
        _inode: u64,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(ENOSYS);
    }

    fn removexattr(&mut self, _request: &Request, _inode: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(ENOSYS);
    }
}

fn timestamp_from_time_or_now(time: TimeOrNow) -> Timestamp {
    match time {
        TimeOrNow::SpecificTime(time) => Timestamp::from_system_time(time),
        TimeOrNow::Now => Timestamp::now(),
    }
}

fn io_error_to_errno(error: io::Error) -> i32 {
    if let Some(errno) = error.raw_os_error() {
        return errno;
    }

    match error.kind() {
        io::ErrorKind::NotFound => ENOENT,
        io::ErrorKind::PermissionDenied => EACCES,
        io::ErrorKind::AlreadyExists => EEXIST,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => EINVAL,
        io::ErrorKind::Unsupported => EOPNOTSUPP,
        _ => EIO,
    }
}

impl Drop for MossFS {
    fn drop(&mut self) {
        self.sync_all_pending();
    }
}

fn current_uid() -> u32 {
    if let Ok(uid_str) = std::env::var("SUDO_UID") {
        if let Ok(uid) = uid_str.parse::<u32>() {
            return uid;
        }
    }
    unsafe { libc::getuid() }
}

fn current_gid() -> u32 {
    if let Ok(gid_str) = std::env::var("SUDO_GID") {
        if let Ok(gid) = gid_str.parse::<u32>() {
            return gid;
        }
    }
    unsafe { libc::getgid() }
}

