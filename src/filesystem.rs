use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_write_file::AtomicWriteFile;

use crate::error::{Result, RomeroError};
use crate::ordering;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryEntry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileMetadata {
    pub kind: EntryKind,
    pub len: u64,
    pub modified_ns: i64,
}

pub(crate) trait FileSystem {
    fn canonicalize(&self, path: &Path) -> Result<Option<PathBuf>>;
    fn read_directory(&self, path: &Path) -> Result<Vec<DirectoryEntry>>;
    fn create_directory_all(&self, path: &Path) -> Result<()>;
    fn metadata(&self, path: &Path) -> Result<Option<FileMetadata>>;
    fn open_reader(&self, path: &Path) -> Result<Box<dyn Read>>;
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
    fn rename(&self, source: &Path, destination: &Path) -> Result<()>;
    fn copy(&self, source: &Path, destination: &Path) -> Result<u64>;
    fn remove_file(&self, path: &Path) -> Result<()>;
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OsFileSystem;

impl FileSystem for OsFileSystem {
    fn canonicalize(&self, path: &Path) -> Result<Option<PathBuf>> {
        match fs::canonicalize(path) {
            Ok(path) => Ok(Some(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RomeroError::io_path(
                "cannot canonicalize path",
                path,
                error,
            )),
        }
    }

    fn read_directory(&self, path: &Path) -> Result<Vec<DirectoryEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)
            .map_err(|error| RomeroError::io_path("cannot read directory", path, error))?
        {
            let entry = entry.map_err(|error| {
                RomeroError::io_path("cannot read directory entry in", path, error)
            })?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path).map_err(|error| {
                RomeroError::io_path("cannot inspect directory entry", &entry_path, error)
            })?;
            entries.push(DirectoryEntry {
                path: entry_path,
                name: entry.file_name(),
                kind: classify(&metadata),
            });
        }
        entries.sort_by(|left, right| ordering::os(&left.name, &right.name));
        Ok(entries)
    }

    fn create_directory_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path)
            .map_err(|error| RomeroError::io_path("cannot create directory", path, error))
    }

    fn metadata(&self, path: &Path) -> Result<Option<FileMetadata>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(FileMetadata {
                kind: classify(&metadata),
                len: metadata.len(),
                modified_ns: metadata
                    .modified()
                    .ok()
                    .map(system_time_ns)
                    .unwrap_or_default(),
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RomeroError::io_path("cannot inspect path", path, error)),
        }
    }

    fn open_reader(&self, path: &Path) -> Result<Box<dyn Read>> {
        File::open(path)
            .map(|file| Box::new(file) as Box<dyn Read>)
            .map_err(|error| RomeroError::io_path("cannot open file", path, error))
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(path).map_err(|error| RomeroError::io_path("cannot read file", path, error))
    }

    fn rename(&self, source: &Path, destination: &Path) -> Result<()> {
        fs::rename(source, destination).map_err(|error| {
            RomeroError::io(
                format!(
                    "cannot move {} to {}",
                    source.display(),
                    destination.display()
                ),
                error,
            )
        })
    }

    fn copy(&self, source: &Path, destination: &Path) -> Result<u64> {
        fs::copy(source, destination).map_err(|error| {
            RomeroError::io(
                format!(
                    "cannot copy {} to {}",
                    source.display(),
                    destination.display()
                ),
                error,
            )
        })
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path)
            .map_err(|error| RomeroError::io_path("cannot remove file", path, error))
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let mut file = AtomicWriteFile::open(path)
            .map_err(|error| RomeroError::io_path("cannot create atomic file", path, error))?;
        file.write_all(contents)
            .map_err(|error| RomeroError::io_path("cannot write atomic file", path, error))?;
        file.commit()
            .map_err(|error| RomeroError::io_path("cannot commit atomic file", path, error))
    }
}

fn classify(metadata: &fs::Metadata) -> EntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::Other
    }
}

fn system_time_ns(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
pub(crate) use memory::MemoryFileSystem;

#[cfg(test)]
mod memory {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use super::*;

    #[derive(Clone, Debug)]
    enum Node {
        File { contents: Vec<u8>, modified_ns: i64 },
        Directory,
        Symlink,
        Other,
    }

    #[derive(Debug, Default)]
    pub(crate) struct MemoryFileSystem {
        nodes: RefCell<BTreeMap<PathBuf, Node>>,
        clock: Cell<i64>,
        fail_next_hash: Cell<bool>,
        fail_next_read: Cell<bool>,
        fail_next_rename: Cell<bool>,
        fail_next_write: Cell<bool>,
        read_directory_calls: Cell<usize>,
        metadata_calls: Cell<usize>,
        read_calls: Cell<usize>,
    }

    impl MemoryFileSystem {
        pub(crate) fn new(root: &Path) -> Self {
            let filesystem = Self::default();
            filesystem.add_directory(root);
            filesystem
        }

        pub(crate) fn add_directory(&self, path: impl AsRef<Path>) {
            self.nodes
                .borrow_mut()
                .insert(path.as_ref().to_path_buf(), Node::Directory);
        }

        pub(crate) fn add_file(&self, path: impl AsRef<Path>, contents: impl Into<Vec<u8>>) {
            let modified_ns = self.tick();
            self.nodes.borrow_mut().insert(
                path.as_ref().to_path_buf(),
                Node::File {
                    contents: contents.into(),
                    modified_ns,
                },
            );
        }

        pub(crate) fn add_symlink(&self, path: impl AsRef<Path>) {
            self.nodes
                .borrow_mut()
                .insert(path.as_ref().to_path_buf(), Node::Symlink);
        }

        pub(crate) fn add_other(&self, path: impl AsRef<Path>) {
            self.nodes
                .borrow_mut()
                .insert(path.as_ref().to_path_buf(), Node::Other);
        }

        pub(crate) fn contents(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
            match self.nodes.borrow().get(path.as_ref()) {
                Some(Node::File { contents, .. }) => Some(contents.clone()),
                _ => None,
            }
        }

        pub(crate) fn contains(&self, path: impl AsRef<Path>) -> bool {
            self.nodes.borrow().contains_key(path.as_ref())
        }

        pub(crate) fn fail_next_rename(&self) {
            self.fail_next_rename.set(true);
        }

        pub(crate) fn fail_next_hash(&self) {
            self.fail_next_hash.set(true);
        }

        pub(crate) fn fail_next_read(&self) {
            self.fail_next_read.set(true);
        }

        pub(crate) fn fail_next_write(&self) {
            self.fail_next_write.set(true);
        }

        pub(crate) fn read_directory_calls(&self) -> usize {
            self.read_directory_calls.get()
        }

        pub(crate) fn metadata_calls(&self) -> usize {
            self.metadata_calls.get()
        }

        pub(crate) fn read_calls(&self) -> usize {
            self.read_calls.get()
        }

        fn tick(&self) -> i64 {
            let next = self.clock.get() + 1;
            self.clock.set(next);
            next
        }
    }

    impl FileSystem for MemoryFileSystem {
        fn canonicalize(&self, path: &Path) -> Result<Option<PathBuf>> {
            Ok(self
                .nodes
                .borrow()
                .contains_key(path)
                .then(|| path.to_path_buf()))
        }

        fn read_directory(&self, path: &Path) -> Result<Vec<DirectoryEntry>> {
            self.read_directory_calls
                .set(self.read_directory_calls.get() + 1);
            if !matches!(self.nodes.borrow().get(path), Some(Node::Directory)) {
                return Err(RomeroError::Operational(format!(
                    "memory path is not a directory: {}",
                    path.display()
                )));
            }
            let mut entries = Vec::new();
            for (entry_path, node) in self.nodes.borrow().iter() {
                if entry_path.parent() == Some(path) {
                    entries.push(DirectoryEntry {
                        path: entry_path.clone(),
                        name: entry_path
                            .file_name()
                            .expect("child path has a name")
                            .to_os_string(),
                        kind: node_kind(node),
                    });
                }
            }
            entries.sort_by(|left, right| ordering::os(&left.name, &right.name));
            Ok(entries)
        }

        fn create_directory_all(&self, path: &Path) -> Result<()> {
            let mut current = PathBuf::new();
            for component in path.components() {
                current.push(component);
                let mut nodes = self.nodes.borrow_mut();
                match nodes.get(&current) {
                    Some(Node::Directory) => {}
                    Some(_) => {
                        return Err(RomeroError::Operational(format!(
                            "memory path is not a directory: {}",
                            current.display()
                        )));
                    }
                    None => {
                        nodes.insert(current.clone(), Node::Directory);
                    }
                }
            }
            Ok(())
        }

        fn metadata(&self, path: &Path) -> Result<Option<FileMetadata>> {
            self.metadata_calls.set(self.metadata_calls.get() + 1);
            Ok(self.nodes.borrow().get(path).map(|node| match node {
                Node::File {
                    contents,
                    modified_ns,
                } => FileMetadata {
                    kind: EntryKind::File,
                    len: contents.len() as u64,
                    modified_ns: *modified_ns,
                },
                Node::Directory => FileMetadata {
                    kind: EntryKind::Directory,
                    len: 0,
                    modified_ns: 0,
                },
                Node::Symlink => FileMetadata {
                    kind: EntryKind::Symlink,
                    len: 0,
                    modified_ns: 0,
                },
                Node::Other => FileMetadata {
                    kind: EntryKind::Other,
                    len: 0,
                    modified_ns: 0,
                },
            }))
        }

        fn open_reader(&self, path: &Path) -> Result<Box<dyn Read>> {
            if self.fail_next_hash.replace(false) {
                return Ok(Box::new(FailingReader));
            }
            self.contents(path)
                .ok_or_else(|| {
                    RomeroError::Operational(format!(
                        "memory path is not a regular file: {}",
                        path.display()
                    ))
                })
                .map(|contents| Box::new(Cursor::new(contents)) as Box<dyn Read>)
        }

        fn read(&self, path: &Path) -> Result<Vec<u8>> {
            self.read_calls.set(self.read_calls.get() + 1);
            if self.fail_next_read.replace(false) {
                return Err(RomeroError::Operational("injected read failure".into()));
            }
            self.contents(path).ok_or_else(|| {
                RomeroError::Operational(format!(
                    "memory path is not a regular file: {}",
                    path.display()
                ))
            })
        }

        fn rename(&self, source: &Path, destination: &Path) -> Result<()> {
            if self.fail_next_rename.replace(false) {
                return Err(RomeroError::Operational("injected rename failure".into()));
            }
            if self.nodes.borrow().contains_key(destination) {
                return Err(RomeroError::Operational(format!(
                    "memory destination exists: {}",
                    destination.display()
                )));
            }
            let mut nodes = self.nodes.borrow_mut();
            let Some(source_node) = nodes.get(source).cloned() else {
                return Err(RomeroError::Operational(format!(
                    "memory source does not exist: {}",
                    source.display()
                )));
            };
            let subtree: Vec<_> = nodes
                .iter()
                .filter(|(path, _)| **path == source || path.starts_with(source))
                .map(|(path, node)| (path.clone(), node.clone()))
                .collect();
            for (path, _) in &subtree {
                nodes.remove(path);
            }
            for (path, node) in subtree {
                let suffix = path.strip_prefix(source).expect("subtree path has prefix");
                nodes.insert(destination.join(suffix), node);
            }
            if !nodes.contains_key(destination) {
                nodes.insert(destination.to_path_buf(), source_node);
            }
            Ok(())
        }

        fn copy(&self, source: &Path, destination: &Path) -> Result<u64> {
            if self.nodes.borrow().contains_key(destination) {
                return Err(RomeroError::Operational(format!(
                    "memory destination exists: {}",
                    destination.display()
                )));
            }
            let contents = self.read(source)?;
            let len = contents.len() as u64;
            self.add_file(destination, contents);
            Ok(len)
        }

        fn remove_file(&self, path: &Path) -> Result<()> {
            let removed = self.nodes.borrow_mut().remove(path);
            if matches!(removed, Some(Node::File { .. })) {
                Ok(())
            } else {
                Err(RomeroError::Operational(format!(
                    "memory path is not a removable file: {}",
                    path.display()
                )))
            }
        }

        fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<()> {
            if self.fail_next_write.replace(false) {
                return Err(RomeroError::Operational("injected write failure".into()));
            }
            if matches!(self.nodes.borrow().get(path), Some(Node::Directory)) {
                return Err(RomeroError::Operational(format!(
                    "memory destination is a directory: {}",
                    path.display()
                )));
            }
            self.add_file(path, contents.to_vec());
            Ok(())
        }
    }

    fn node_kind(node: &Node) -> EntryKind {
        match node {
            Node::File { .. } => EntryKind::File,
            Node::Directory => EntryKind::Directory,
            Node::Symlink => EntryKind::Symlink,
            Node::Other => EntryKind::Other,
        }
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected hash failure"))
        }
    }
}
