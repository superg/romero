use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::error::{Result, RomeroError};
use crate::filesystem::{EntryKind, FileSystem, OsFileSystem};

const CONFIG_NAME: &str = "romero.yaml";
const DATABASE_NAME: &str = ".romero.sqlite3";

fn default_library_path() -> PathBuf {
    PathBuf::from("library")
}

fn default_work_path() -> PathBuf {
    PathBuf::from("work")
}

fn default_dat_path() -> PathBuf {
    PathBuf::from("dats")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigValues {
    #[serde(deserialize_with = "deserialize_path")]
    pub library_path: PathBuf,
    #[serde(deserialize_with = "deserialize_path")]
    pub work_path: PathBuf,
    #[serde(deserialize_with = "deserialize_path")]
    pub dat_path: PathBuf,
}

fn deserialize_path<'de, D>(deserializer: D) -> std::result::Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(PathBuf::from)
}

impl Default for ConfigValues {
    fn default() -> Self {
        Self {
            library_path: default_library_path(),
            work_path: default_work_path(),
            dat_path: default_dat_path(),
        }
    }
}

impl ConfigValues {
    pub fn from_yaml(yaml: Option<&str>) -> Result<Self> {
        let Some(yaml) = yaml else {
            return Ok(Self::default());
        };
        if yaml.trim().is_empty() {
            return Ok(Self::default());
        }

        let options = serde_saphyr::options! {
            no_schema: true,
        };
        serde_saphyr::from_str_with_options(yaml, options)
            .map_err(|error| RomeroError::Config(format!("invalid {CONFIG_NAME}: {error}")))
    }

    pub fn validate(&self) -> Result<()> {
        let library = normalize_relative("library_path", &self.library_path)?;
        let work = normalize_relative("work_path", &self.work_path)?;
        let dat = normalize_relative("dat_path", &self.dat_path)?;
        let paths = [
            ("library_path", library.as_path()),
            ("work_path", work.as_path()),
            ("dat_path", dat.as_path()),
        ];

        for (name, path) in paths {
            let first = path
                .components()
                .next()
                .map(|component| component.as_os_str().to_string_lossy().to_lowercase());
            if first.as_deref() == Some(CONFIG_NAME) || first.as_deref() == Some(DATABASE_NAME) {
                return Err(RomeroError::Config(format!(
                    "{name} must not contain {CONFIG_NAME} or {DATABASE_NAME}"
                )));
            }
        }

        for (index, (left_name, left)) in paths.iter().enumerate() {
            for (right_name, right) in paths.iter().skip(index + 1) {
                if paths_overlap(left, right) {
                    return Err(RomeroError::Config(format!(
                        "{left_name} and {right_name} overlap"
                    )));
                }
            }
        }

        Ok(())
    }

    fn normalized(&self) -> Result<Self> {
        self.validate()?;
        Ok(Self {
            library_path: normalize_relative("library_path", &self.library_path)?,
            work_path: normalize_relative("work_path", &self.work_path)?,
            dat_path: normalize_relative("dat_path", &self.dat_path)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfig {
    pub root: PathBuf,
    pub library_path: PathBuf,
    pub work_path: PathBuf,
    pub dat_path: PathBuf,
    pub database_path: PathBuf,
}

impl ResolvedConfig {
    pub fn load(root: &Path) -> Result<Self> {
        Self::load_with_filesystem(&OsFileSystem, root)
    }

    fn load_with_filesystem<F: FileSystem>(filesystem: &F, root: &Path) -> Result<Self> {
        let Some(canonical_root) = filesystem.canonicalize(root)? else {
            return Err(RomeroError::InvalidRoot(format!(
                "root does not exist: {}",
                root.display()
            )));
        };
        let root_metadata = filesystem.metadata(&canonical_root)?.ok_or_else(|| {
            RomeroError::InvalidRoot(format!("root does not exist: {}", root.display()))
        })?;
        if root_metadata.kind != EntryKind::Directory {
            return Err(RomeroError::InvalidRoot(format!(
                "root is not a directory: {}",
                root.display()
            )));
        }

        let root = canonical_root;
        let config_path = root.join(CONFIG_NAME);
        let yaml = match filesystem.metadata(&config_path)? {
            Some(metadata) => {
                if metadata.kind != EntryKind::File {
                    return Err(RomeroError::Config(format!(
                        "{CONFIG_NAME} must be a regular file"
                    )));
                }
                let bytes = filesystem.read(&config_path)?;
                Some(String::from_utf8(bytes).map_err(|error| {
                    RomeroError::io_path(
                        "cannot read configuration",
                        &config_path,
                        io::Error::new(io::ErrorKind::InvalidData, error),
                    )
                })?)
            }
            None => None,
        };

        let values = ConfigValues::from_yaml(yaml.as_deref())?.normalized()?;
        let library_path = root.join(&values.library_path);
        let work_path = root.join(&values.work_path);
        let dat_path = root.join(&values.dat_path);
        for path in [&library_path, &work_path, &dat_path] {
            inspect_managed_directory(filesystem, &root, path)?;
        }

        Ok(Self {
            database_path: root.join(DATABASE_NAME),
            root,
            library_path,
            work_path,
            dat_path,
        })
    }

    pub(crate) fn prepare_managed_directories(&self) -> Result<()> {
        self.prepare_managed_directories_with(&OsFileSystem)
    }

    fn prepare_managed_directories_with<F: FileSystem>(&self, filesystem: &F) -> Result<()> {
        for path in [&self.library_path, &self.work_path, &self.dat_path] {
            inspect_managed_directory(filesystem, &self.root, path)?;
        }
        for path in [&self.library_path, &self.work_path, &self.dat_path] {
            filesystem.create_directory_all(path)?;
        }
        Ok(())
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let folded = |path: &Path| {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
    };
    let left = folded(left);
    let right = folded(right);
    left.starts_with(&right) || right.starts_with(&left)
}

fn normalize_relative(name: &str, path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    let has_portable_prefix = text.starts_with(r"\\")
        || text
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':' && text.as_bytes()[0].is_ascii_alphabetic());
    if path.as_os_str().is_empty() || path.is_absolute() || has_portable_prefix {
        return Err(RomeroError::Config(format!(
            "{name} must be a nonempty relative path"
        )));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RomeroError::Config(format!(
                    "{name} must remain inside the Romero root"
                )));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(RomeroError::Config(format!(
            "{name} must not resolve to the Romero root"
        )));
    }
    Ok(normalized)
}

fn inspect_managed_directory<F: FileSystem>(
    filesystem: &F,
    root: &Path,
    path: &Path,
) -> Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        RomeroError::Config(format!(
            "managed path escapes the Romero root: {}",
            path.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            unreachable!("relative path was normalized before directory preparation");
        };
        current.push(component);
        match filesystem.metadata(&current)? {
            Some(metadata) if metadata.kind == EntryKind::Symlink => {
                return Err(RomeroError::Config(format!(
                    "managed path contains a symlink: {}",
                    current.display()
                )));
            }
            Some(metadata) if metadata.kind != EntryKind::Directory => {
                return Err(RomeroError::Config(format!(
                    "managed path component is not a directory: {}",
                    current.display()
                )));
            }
            Some(_) | None => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::filesystem::MemoryFileSystem;

    use super::*;

    #[test]
    fn missing_yaml_uses_defaults() {
        assert_eq!(
            ConfigValues::from_yaml(None).unwrap(),
            ConfigValues::default()
        );
    }

    #[test]
    fn empty_yaml_uses_defaults() {
        assert_eq!(
            ConfigValues::from_yaml(Some(" \n")).unwrap(),
            ConfigValues::default()
        );
        assert_eq!(
            ConfigValues::from_yaml(Some("{}\n")).unwrap(),
            ConfigValues::default()
        );
    }

    #[test]
    fn partial_yaml_merges_defaults() {
        let config = ConfigValues::from_yaml(Some("work_path: intake\n")).unwrap();
        assert_eq!(config.work_path, Path::new("intake"));
        assert_eq!(config.library_path, Path::new("library"));
        assert_eq!(config.dat_path, Path::new("dats"));
    }

    #[test]
    fn unknown_yaml_key_is_rejected() {
        let error = ConfigValues::from_yaml(Some("cache_path: cache\n")).unwrap_err();
        assert!(error.to_string().contains("unknown"));
        assert!(ConfigValues::from_yaml(Some("work_path: 42\n")).is_err());
    }

    #[test]
    fn unsafe_and_overlapping_paths_are_rejected() {
        for path in [
            "",
            ".",
            "../library",
            "/library",
            r"C:\library",
            r"\\server\share",
        ] {
            let config = ConfigValues {
                library_path: PathBuf::from(path),
                ..ConfigValues::default()
            };
            assert!(config.validate().is_err(), "{path:?} should fail");
        }

        let config = ConfigValues {
            library_path: PathBuf::from("storage"),
            work_path: PathBuf::from("storage/work"),
            ..ConfigValues::default()
        };
        assert!(config.validate().is_err());

        let case_collision = ConfigValues {
            library_path: PathBuf::from("Storage"),
            work_path: PathBuf::from("storage/work"),
            ..ConfigValues::default()
        };
        assert!(case_collision.validate().is_err());
    }

    #[test]
    fn nested_distinct_paths_are_accepted() {
        let config = ConfigValues {
            library_path: PathBuf::from("storage/library"),
            work_path: PathBuf::from("storage/work"),
            dat_path: PathBuf::from("metadata/dats"),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn managed_paths_cannot_contain_reserved_root_files() {
        for path in [
            "romero.yaml/inside",
            "ROMERO.YAML/inside",
            ".romero.sqlite3/inside",
        ] {
            let config = ConfigValues {
                library_path: PathBuf::from(path),
                ..ConfigValues::default()
            };
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn loads_and_prepares_default_paths_in_memory() {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));

        let config = ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root")).unwrap();

        assert_eq!(
            config,
            ResolvedConfig {
                root: PathBuf::from("/root"),
                library_path: PathBuf::from("/root/library"),
                work_path: PathBuf::from("/root/work"),
                dat_path: PathBuf::from("/root/dats"),
                database_path: PathBuf::from("/root/.romero.sqlite3"),
            }
        );
        assert!(!filesystem.contains("/root/library"));
        assert!(!filesystem.contains("/root/work"));
        assert!(!filesystem.contains("/root/dats"));

        config
            .prepare_managed_directories_with(&filesystem)
            .unwrap();

        assert!(filesystem.contains("/root/library"));
        assert!(filesystem.contains("/root/work"));
        assert!(filesystem.contains("/root/dats"));
    }

    #[test]
    fn loads_partial_configuration_and_rejects_invalid_utf8_in_memory() {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        filesystem.add_file("/root/romero.yaml", b"work_path: intake\n".to_vec());

        let config = ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root")).unwrap();

        assert_eq!(config.library_path, Path::new("/root/library"));
        assert_eq!(config.work_path, Path::new("/root/intake"));
        assert_eq!(config.dat_path, Path::new("/root/dats"));

        filesystem.add_file("/root/romero.yaml", vec![0xff]);
        assert!(
            ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root"))
                .unwrap_err()
                .to_string()
                .contains("cannot read configuration")
        );
    }

    #[test]
    fn rejects_missing_and_non_directory_roots_in_memory() {
        let filesystem = MemoryFileSystem::new(Path::new("/elsewhere"));
        let missing =
            ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root")).unwrap_err();
        assert_eq!(missing.to_string(), "root does not exist: /root");

        filesystem.add_file("/root", Vec::new());
        let file_root =
            ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root")).unwrap_err();
        assert_eq!(file_root.to_string(), "root is not a directory: /root");
    }

    #[test]
    fn rejects_non_file_configuration_and_unsafe_managed_components_in_memory() {
        let directory_config = MemoryFileSystem::new(Path::new("/root"));
        directory_config.add_directory("/root/romero.yaml");
        assert!(
            ResolvedConfig::load_with_filesystem(&directory_config, Path::new("/root"))
                .unwrap_err()
                .to_string()
                .contains("must be a regular file")
        );

        let symlink_config = MemoryFileSystem::new(Path::new("/root"));
        symlink_config.add_symlink("/root/romero.yaml");
        assert!(
            ResolvedConfig::load_with_filesystem(&symlink_config, Path::new("/root"))
                .unwrap_err()
                .to_string()
                .contains("must be a regular file")
        );

        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        filesystem.add_file(
            "/root/romero.yaml",
            b"library_path: linked/library\n".to_vec(),
        );
        filesystem.add_symlink("/root/linked");

        assert!(
            ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root"))
                .unwrap_err()
                .to_string()
                .contains("managed path contains a symlink")
        );
        assert!(!filesystem.contains("/root/linked/library"));

        filesystem.add_file("/root/linked", Vec::new());
        assert!(
            ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root"))
                .unwrap_err()
                .to_string()
                .contains("managed path component is not a directory")
        );
    }

    #[test]
    fn invalid_configuration_does_not_prepare_directories_in_memory() {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        filesystem.add_file("/root/romero.yaml", b"library_path: ../outside\n".to_vec());

        assert!(ResolvedConfig::load_with_filesystem(&filesystem, Path::new("/root")).is_err());
        assert!(!filesystem.contains("/root/library"));
        assert!(!filesystem.contains("/root/work"));
        assert!(!filesystem.contains("/root/dats"));
    }
}
