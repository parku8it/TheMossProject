use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAGIC_NUMBER: &[u8; 4] = b"MOSS";
const INDEX_MAGIC: &[u8; 4] = b"MOSI";
const VERSION: u16 = 2;
const LEGACY_VERSION: u16 = 1;
const HEADER_SIZE: u64 = 64;
const MAX_PATH_LENGTH: usize = 1024 * 1024;
const MAX_ENTRY_COUNT: u64 = 10_000_000;

const CHECKPOINT_MUTATIONS: u64 = 4096;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

pub type SharedMoss = Arc<Mutex<Moss>>;

pub struct IdleCheckpoint {
    dirty: AtomicBool,
    last_mutation: Mutex<Instant>,
}

impl IdleCheckpoint {
    pub fn start(
        archive: &SharedMoss,
        idle_duration: Duration,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            dirty: AtomicBool::new(false),
            last_mutation: Mutex::new(Instant::now()),
        });

        let weak_archive: Weak<Mutex<Moss>> = Arc::downgrade(archive);
        let weak_state = Arc::downgrade(&state);

        thread::Builder::new()
            .name("moss-idle-checkpoint".to_owned())
            .spawn(move || {
                const POLL_INTERVAL: Duration = Duration::from_secs(1);

                loop {
                    thread::sleep(POLL_INTERVAL);

                    let Some(state) = weak_state.upgrade() else {
                        break;
                    };

                    if !state.dirty.load(Ordering::Acquire) {
                        continue;
                    }

                    let idle_for = match state.last_mutation.lock() {
                        Ok(last_mutation) => last_mutation.elapsed(),
                        Err(poisoned) => poisoned.into_inner().elapsed(),
                    };

                    if idle_for < idle_duration {
                        continue;
                    }

                    let Some(archive) = weak_archive.upgrade() else {
                        break;
                    };

                    let checkpoint_result = match archive.lock() {
                        Ok(mut archive) => archive.checkpoint(),
                        Err(poisoned) => {
                            let mut archive = poisoned.into_inner();
                            archive.checkpoint()
                        }
                    };

                    if checkpoint_result.is_ok() {

                        state.dirty.store(false, Ordering::Release);
                    }
                }
            })
            .expect("failed to start Moss idle-checkpoint thread");

        state
    }

    pub fn mark_mutated(&self) {
        match self.last_mutation.lock() {
            Ok(mut last_mutation) => {
                *last_mutation = Instant::now();
            }
            Err(poisoned) => {
                *poisoned.into_inner() = Instant::now();
            }
        }

        self.dirty.store(true, Ordering::Release);
    }

    pub fn force(&self, archive: &SharedMoss) -> io::Result<()> {
        let result = match archive.lock() {
            Ok(mut archive) => archive.sync(),
            Err(poisoned) => {
                let mut archive = poisoned.into_inner();
                archive.sync()
            }
        };

        if result.is_ok() {
            self.dirty.store(false, Ordering::Release);
        }

        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

impl EntryKind {
    fn to_byte(self) -> u8 {
        match self {
            Self::File => 0,
            Self::Directory => 1,
            Self::Symlink => 2,
        }
    }

    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::File),
            1 => Ok(Self::Directory),
            2 => Ok(Self::Symlink),
            _ => Err(invalid_data("Invalid entry type in Moss index")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

impl Timestamp {
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self {
                seconds: duration.as_secs().min(i64::MAX as u64) as i64,
                nanoseconds: duration.subsec_nanos(),
            },
            Err(error) => {
                let duration = error.duration();
                if duration.subsec_nanos() == 0 {
                    Self {
                        seconds: -(duration.as_secs().min(i64::MAX as u64) as i64),
                        nanoseconds: 0,
                    }
                } else {
                    Self {
                        seconds: -(duration.as_secs().min(i64::MAX as u64) as i64) - 1,
                        nanoseconds: 1_000_000_000 - duration.subsec_nanos(),
                    }
                }
            }
        }
    }

    pub fn to_system_time(self) -> SystemTime {
        if self.seconds >= 0 {
            UNIX_EPOCH
                .checked_add(std::time::Duration::new(
                    self.seconds as u64,
                    self.nanoseconds.min(999_999_999),
                ))
                .unwrap_or(UNIX_EPOCH)
        } else if self.nanoseconds == 0 {
            UNIX_EPOCH
                .checked_sub(std::time::Duration::from_secs(self.seconds.unsigned_abs()))
                .unwrap_or(UNIX_EPOCH)
        } else {
            let seconds = self.seconds.unsigned_abs().saturating_sub(1);
            let nanoseconds = 1_000_000_000 - self.nanoseconds.min(999_999_999);
            UNIX_EPOCH
                .checked_sub(std::time::Duration::new(seconds, nanoseconds))
                .unwrap_or(UNIX_EPOCH)
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub virtual_path: String,
    pub kind: EntryKind,
    pub start_byte: u64,
    pub block_length: u64,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
}

impl IndexEntry {
    pub fn new_file(path: String, mode: u16, uid: u32, gid: u32) -> Self {
        let now = Timestamp::now();
        Self {
            virtual_path: path,
            kind: EntryKind::File,
            start_byte: 0,
            block_length: 0,
            mode,
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
        }
    }

    pub fn new_directory(path: String, mode: u16, uid: u32, gid: u32) -> Self {
        let now = Timestamp::now();
        Self {
            virtual_path: path,
            kind: EntryKind::Directory,
            start_byte: 0,
            block_length: 0,
            mode,
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
        }
    }

    pub fn new_symlink(path: String, uid: u32, gid: u32) -> Self {
        let now = Timestamp::now();
        Self {
            virtual_path: path,
            kind: EntryKind::Symlink,
            start_byte: 0,
            block_length: 0,
            mode: 0o777,
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
        }
    }
}

pub struct Moss {
    path: PathBuf,
    file: File,
    entries: BTreeMap<String, IndexEntry>,
    index_pointer: u64,
    index_dirty: bool,
    mutations_since_checkpoint: u64,
    last_checkpoint: Instant,
}

impl Moss {
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        file.write_all(MAGIC_NUMBER)?;
        file.write_all(&VERSION.to_le_bytes())?;
        file.write_all(&HEADER_SIZE.to_le_bytes())?;
        file.write_all(&[0_u8; 50])?;
        file.sync_data()?;

        let mut stor = Self {
            path,
	    file,
	    entries: BTreeMap::new(),
	    index_pointer: HEADER_SIZE,
	    index_dirty: false,
	    mutations_since_checkpoint: 0,
	    last_checkpoint: Instant::now(),
	};
        stor.flush_index()?;
        Ok(stor)
    }

    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;

        if file.metadata()?.len() < HEADER_SIZE {
            return Err(invalid_data("Moss file is smaller than its header"));
        }

        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic)?;

        if &magic != MAGIC_NUMBER {
            return Err(invalid_data("Invalid Moss file signature"));
        }

        let version = read_u16(&mut file)?;

        if version != VERSION && version != LEGACY_VERSION {
            return Err(invalid_data(format!(
                "Unsupported Moss version: {version}"
            )));
        }

        let index_pointer = read_u64(&mut file)?;
        let file_length = file.metadata()?.len();

        if index_pointer < HEADER_SIZE || index_pointer > file_length {
            return Err(invalid_data("Moss index pointer is out of range"));
        }

        let mut stor = Self {
            path,
	    file,
	    entries: BTreeMap::new(),
	    index_pointer,
	    index_dirty: false,
	    mutations_since_checkpoint: 0,
	    last_checkpoint: Instant::now(),
	};

        if version == LEGACY_VERSION {
            stor.load_legacy_index()?;
        } else {
            stor.load_index()?;
        }

        Ok(stor)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> impl Iterator<Item = &IndexEntry> {
        self.entries.values()
    }

    pub fn get_entry(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.get(path)
    }

    pub fn contains_path(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    pub fn update_metadata(
        &mut self,
        virtual_path: &str,
        mode: Option<u16>,
        atime: Option<Timestamp>,
        mtime: Option<Timestamp>,
    ) -> io::Result<()> {
        let path = normalize_virtual_path(virtual_path)?;
        let entry = self.entries.get_mut(&path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Storage entry not found")
        })?;

        if let Some(mode) = mode {
            entry.mode = mode;
        }
        if let Some(atime) = atime {
            entry.atime = atime;
        }
        if let Some(mtime) = mtime {
            entry.mtime = mtime;
        }
        entry.ctime = Timestamp::now();
        self.mark_index_dirty()
    }

    fn mark_index_dirty(&mut self) -> io::Result<()> {
		self.index_dirty = true;
		self.mutations_since_checkpoint =
			self.mutations_since_checkpoint.saturating_add(1);

		if self.mutations_since_checkpoint >= CHECKPOINT_MUTATIONS
			|| self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL
		{
			self.flush_index()?;
		}

		Ok(())
	}

	pub fn checkpoint(&mut self) -> io::Result<()> {
		if self.index_dirty {
			self.flush_index()?;
		}
		Ok(())
	}

    pub fn sync(&mut self) -> io::Result<()> {
		self.checkpoint()?;
		self.file.sync_all()
	}

    pub fn read_file(&mut self, virtual_path: &str) -> io::Result<Vec<u8>> {
        let entry = self.entries.get(virtual_path).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Storage entry not found")
        })?;

        if entry.kind == EntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot read a directory as a file",
            ));
        }

        let length = usize::try_from(entry.block_length)
            .map_err(|_| invalid_data("Storage entry is too large for this platform"))?;

        let mut data = vec![0_u8; length];
        self.file.seek(SeekFrom::Start(entry.start_byte))?;
        self.file.read_exact(&mut data)?;
        Ok(data)
    }

    pub fn write_file(&mut self, virtual_path: &str, data: &[u8]) -> io::Result<()> {
        let path = normalize_virtual_path(virtual_path)?;
        let mut entry = self.entries.get(&path).cloned().unwrap_or_else(|| {
            IndexEntry::new_file(path.clone(), 0o644, current_uid(), current_gid())
        });

        if entry.kind == EntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot replace a directory with file data",
            ));
        }

        entry.virtual_path = path;
        entry.kind = EntryKind::File;
        entry.mtime = Timestamp::now();
        entry.ctime = entry.mtime;

        self.upsert_entry(entry, Some(data))
    }

    pub fn upsert_entry(&mut self, mut entry: IndexEntry, data: Option<&[u8]>) -> io::Result<()> {
        entry.virtual_path = normalize_virtual_path(&entry.virtual_path)?;

        if entry.kind == EntryKind::Directory {
            entry.start_byte = 0;
            entry.block_length = 0;
        } else if let Some(data) = data {
            let payload_position = self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(data)?;
            entry.start_byte = payload_position;
            entry.block_length = data.len() as u64;
        } else if let Some(previous) = self.entries.get(&entry.virtual_path) {
            entry.start_byte = previous.start_byte;
            entry.block_length = previous.block_length;
        } else if entry.block_length != 0 {
            return Err(invalid_data(
                "A new non-empty entry must include payload data",
            ));
        }

        self.entries.insert(entry.virtual_path.clone(), entry);
		self.mark_index_dirty()
    }

    pub fn remove_prefix(&mut self, path: &str) -> io::Result<usize> {
        let path = normalize_virtual_path(path)?;
        let prefix = format!("{path}/");

        let targets: Vec<String> = self
            .entries
            .keys()
            .filter(|candidate| *candidate == &path || candidate.starts_with(&prefix))
            .cloned()
            .collect();

        let count = targets.len();

        for target in targets {
            self.entries.remove(&target);
        }

        if count != 0 {
			self.mark_index_dirty()?;
		}

        Ok(count)
    }

    pub fn rename_prefix(&mut self, old_path: &str, new_path: &str) -> io::Result<()> {
        let old_path = normalize_virtual_path(old_path)?;
        let new_path = normalize_virtual_path(new_path)?;

        if old_path == new_path {
            return Ok(());
        }

        let old_prefix = format!("{old_path}/");
        let new_prefix = format!("{new_path}/");

        if new_path.starts_with(&old_prefix) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot move a directory into itself",
            ));
        }

        let moved: Vec<(String, IndexEntry)> = self
            .entries
            .iter()
            .filter(|(path, _)| *path == &old_path || path.starts_with(&old_prefix))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();

        if moved.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Storage entry not found",
            ));
        }

        for (old, _) in &moved {
            self.entries.remove(old);
        }

        for (old, mut entry) in moved {
            let suffix = old.strip_prefix(&old_path).unwrap_or_default();
            let target = if suffix.is_empty() {
                new_path.clone()
            } else {
                format!("{new_prefix}{}", suffix.trim_start_matches('/'))
            };

            entry.virtual_path = target.clone();
            entry.ctime = Timestamp::now();
            self.entries.insert(target, entry);
        }

        self.mark_index_dirty()
    }

    fn load_index(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(self.index_pointer))?;

        let mut magic = [0_u8; 4];
        self.file.read_exact(&mut magic)?;

        if &magic != INDEX_MAGIC {
            return Err(invalid_data("Invalid Moss index signature"));
        }

        let count = read_u64(&mut self.file)?;

        if count > MAX_ENTRY_COUNT {
            return Err(invalid_data("Moss index contains too many entries"));
        }

        self.entries.clear();

        for _ in 0..count {
            let path_length = read_u32(&mut self.file)? as usize;

            if path_length == 0 || path_length > MAX_PATH_LENGTH {
                return Err(invalid_data("Invalid virtual path length"));
            }

            let mut path_bytes = vec![0_u8; path_length];
            self.file.read_exact(&mut path_bytes)?;

            let path = String::from_utf8(path_bytes)
                .map_err(|_| invalid_data("Virtual path is not valid UTF-8"))?;
            let path = normalize_virtual_path(&path)?;

            let mut kind = [0_u8; 1];
            self.file.read_exact(&mut kind)?;

            let entry = IndexEntry {
                virtual_path: path.clone(),
                kind: EntryKind::from_byte(kind[0])?,
                start_byte: read_u64(&mut self.file)?,
                block_length: read_u64(&mut self.file)?,
                mode: read_u16(&mut self.file)?,
                uid: read_u32(&mut self.file)?,
                gid: read_u32(&mut self.file)?,
                atime: read_timestamp(&mut self.file)?,
                mtime: read_timestamp(&mut self.file)?,
                ctime: read_timestamp(&mut self.file)?,
            };

            if entry.kind != EntryKind::Directory {
                let end = entry
                    .start_byte
                    .checked_add(entry.block_length)
                    .ok_or_else(|| invalid_data("Storage entry range overflow"))?;

                if end > self.index_pointer {
                    return Err(invalid_data(
                        "Storage payload overlaps or exceeds the active index",
                    ));
                }
            }

            if self.entries.insert(path, entry).is_some() {
                return Err(invalid_data("Duplicate path in Moss index"));
            }
        }

        Ok(())
    }

    fn load_legacy_index(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(self.index_pointer))?;

        let mut buffer = Vec::new();
        self.file.read_to_end(&mut buffer)?;

        let mut cursor = 0_usize;
        let now = Timestamp::now();

        while cursor < buffer.len() {
            if buffer.len() - cursor < 2 {
                return Err(invalid_data("Truncated legacy index"));
            }

            let path_length = u16::from_le_bytes([buffer[cursor], buffer[cursor + 1]]) as usize;
            cursor += 2;

            let required = path_length
                .checked_add(24)
                .ok_or_else(|| invalid_data("Legacy index length overflow"))?;

            if path_length == 0 || path_length > MAX_PATH_LENGTH || buffer.len() - cursor < required
            {
                return Err(invalid_data("Invalid legacy index entry"));
            }

            let path = String::from_utf8(buffer[cursor..cursor + path_length].to_vec())
                .map_err(|_| invalid_data("Legacy path is not valid UTF-8"))?;
            let path = normalize_virtual_path(&path)?;
            cursor += path_length;

            let start_byte = read_u64_slice(&buffer, &mut cursor)?;
            let block_length = read_u64_slice(&buffer, &mut cursor)?;
            let _legacy_next_chunk = read_u64_slice(&buffer, &mut cursor)?;

            let end = start_byte
                .checked_add(block_length)
                .ok_or_else(|| invalid_data("Legacy entry range overflow"))?;

            if end > self.index_pointer {
                return Err(invalid_data("Legacy entry points outside payload data"));
            }

            self.entries.insert(
                path.clone(),
                IndexEntry {
                    virtual_path: path,
                    kind: EntryKind::File,
                    start_byte,
                    block_length,
                    mode: 0o644,
                    uid: current_uid(),
                    gid: current_gid(),
                    atime: now,
                    mtime: now,
                    ctime: now,
                },
            );
        }

        self.flush_index()
    }

    fn flush_index(&mut self) -> io::Result<()> {
        let mut index = Vec::with_capacity(
            12 + self
                .entries
                .values()
                .map(|entry| entry.virtual_path.len() + 67)
                .sum::<usize>(),
        );

        index.extend_from_slice(INDEX_MAGIC);
        index.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());

        for entry in self.entries.values() {
            let path = entry.virtual_path.as_bytes();

            if path.is_empty() || path.len() > MAX_PATH_LENGTH || path.len() > u32::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Virtual path is too long",
                ));
            }

            index.extend_from_slice(&(path.len() as u32).to_le_bytes());
            index.extend_from_slice(path);
            index.push(entry.kind.to_byte());
            index.extend_from_slice(&entry.start_byte.to_le_bytes());
            index.extend_from_slice(&entry.block_length.to_le_bytes());
            index.extend_from_slice(&entry.mode.to_le_bytes());
            index.extend_from_slice(&entry.uid.to_le_bytes());
            index.extend_from_slice(&entry.gid.to_le_bytes());
            write_timestamp(&mut index, entry.atime);
            write_timestamp(&mut index, entry.mtime);
            write_timestamp(&mut index, entry.ctime);
        }

        let new_index_pointer = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&index)?;
        self.file.sync_data()?;

        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&VERSION.to_le_bytes())?;
        self.file.write_all(&new_index_pointer.to_le_bytes())?;
        self.file.sync_data()?;

        self.index_pointer = new_index_pointer;
		self.index_dirty = false;
		self.mutations_since_checkpoint = 0;
		self.last_checkpoint = Instant::now();
		Ok(())
    }

    pub fn compact(&mut self) -> io::Result<()> {
        self.checkpoint()?;

        let temp_path = self.path.with_extension("tmp");

        let mut new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)?;

        new_file.write_all(MAGIC_NUMBER)?;
        new_file.write_all(&VERSION.to_le_bytes())?;
        new_file.write_all(&HEADER_SIZE.to_le_bytes())?;
        new_file.write_all(&[0_u8; 50])?;

        let mut new_entries: BTreeMap<String, IndexEntry> = BTreeMap::new();

        for (path, entry) in &self.entries {
            let mut new_entry = entry.clone();

            if entry.kind != EntryKind::Directory && entry.block_length > 0 {
                let mut buf = vec![0u8; entry.block_length as usize];
                self.file.seek(SeekFrom::Start(entry.start_byte))?;
                self.file.read_exact(&mut buf)?;

                let payload_offset = new_file.seek(SeekFrom::End(0))?;
                new_file.write_all(&buf)?;

                new_entry.start_byte = payload_offset;
            } else {
                new_entry.start_byte = 0;
                new_entry.block_length = 0;
            }

            new_entries.insert(path.clone(), new_entry);
        }

        let mut index = Vec::with_capacity(
            12 + new_entries
                .values()
                .map(|entry| entry.virtual_path.len() + 67)
                .sum::<usize>(),
        );

        index.extend_from_slice(INDEX_MAGIC);
        index.extend_from_slice(&(new_entries.len() as u64).to_le_bytes());

        for entry in new_entries.values() {
            let path = entry.virtual_path.as_bytes();
            index.extend_from_slice(&(path.len() as u32).to_le_bytes());
            index.extend_from_slice(path);
            index.push(entry.kind.to_byte());
            index.extend_from_slice(&entry.start_byte.to_le_bytes());
            index.extend_from_slice(&entry.block_length.to_le_bytes());
            index.extend_from_slice(&entry.mode.to_le_bytes());
            index.extend_from_slice(&entry.uid.to_le_bytes());
            index.extend_from_slice(&entry.gid.to_le_bytes());
            write_timestamp(&mut index, entry.atime);
            write_timestamp(&mut index, entry.mtime);
            write_timestamp(&mut index, entry.ctime);
        }

        let new_index_pointer = new_file.seek(SeekFrom::End(0))?;
        new_file.write_all(&index)?;
        new_file.sync_data()?;

        new_file.seek(SeekFrom::Start(4))?;
        new_file.write_all(&VERSION.to_le_bytes())?;
        new_file.write_all(&new_index_pointer.to_le_bytes())?;
        new_file.sync_data()?;

        drop(new_file);
        std::fs::rename(&temp_path, &self.path)?;

        self.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;
        self.entries = new_entries;
        self.index_pointer = new_index_pointer;
        self.index_dirty = false;
        self.mutations_since_checkpoint = 0;
        self.last_checkpoint = Instant::now();

        Ok(())
    }
}

pub fn normalize_virtual_path(path: &str) -> io::Result<String> {
    let replaced_path = path.replace('\\', "/");
    let mut normalized = Vec::new();

    for component in replaced_path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Parent traversal is not allowed in virtual paths",
                ));
            }
            component if component.as_bytes().contains(&0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Virtual paths cannot contain NUL bytes",
                ));
            }
            component => normalized.push(component),
        }
    }

    if normalized.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The storage root cannot be used as an entry path",
        ));
    }

    Ok(normalized.join("/"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_uid() -> u32 {
    if let Ok(uid_str) = std::env::var("SUDO_UID") {
        if let Ok(uid) = uid_str.parse::<u32>() {
            return uid;
        }
    }
    unsafe { libc::getuid() }
}

#[cfg(target_os = "windows")]
fn current_uid() -> u32 {
    0
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_gid() -> u32 {
    if let Ok(gid_str) = std::env::var("SUDO_GID") {
        if let Ok(gid) = gid_str.parse::<u32>() {
            return gid;
        }
    }
    unsafe { libc::getgid() }
}

#[cfg(target_os = "windows")]
fn current_gid() -> u32 {
    0
}

fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_i64(reader: &mut impl Read) -> io::Result<i64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(i64::from_le_bytes(bytes))
}

fn read_timestamp(reader: &mut impl Read) -> io::Result<Timestamp> {
    let seconds = read_i64(reader)?;
    let nanoseconds = read_u32(reader)?;

    if nanoseconds >= 1_000_000_000 {
        return Err(invalid_data("Invalid timestamp nanosecond value"));
    }

    Ok(Timestamp {
        seconds,
        nanoseconds,
    })
}

fn write_timestamp(output: &mut Vec<u8>, timestamp: Timestamp) {
    output.extend_from_slice(&timestamp.seconds.to_le_bytes());
    output.extend_from_slice(&timestamp.nanoseconds.to_le_bytes());
}

fn read_u64_slice(buffer: &[u8], cursor: &mut usize) -> io::Result<u64> {
    let end = cursor
        .checked_add(8)
        .ok_or_else(|| invalid_data("Index cursor overflow"))?;

    let bytes: [u8; 8] = buffer
        .get(*cursor..end)
        .ok_or_else(|| invalid_data("Truncated index integer"))?
        .try_into()
        .map_err(|_| invalid_data("Invalid index integer"))?;

    *cursor = end;
    Ok(u64::from_le_bytes(bytes))
}

