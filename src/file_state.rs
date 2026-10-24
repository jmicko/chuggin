//! File observations without version control. Manifests contain hashes, not
//! recoverable copies of file contents; no Git executable or repository is used.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64, u32),
}
impl From<&fs::Metadata> for Stamp {
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    meta.dev(),
                    meta.ino(),
                    meta.ctime(),
                    meta.ctime_nsec(),
                    meta.mode(),
                )
            },
        }
    }
}
type Cache = HashMap<PathBuf, (Stamp, String)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

#[derive(Serialize, Deserialize)]
struct Manifest {
    files: BTreeMap<String, String>,
}

fn entry_hash(path: &Path, meta: &fs::Metadata, cache: &mut Cache) -> String {
    let stamp = Stamp::from(meta);
    if let Some((previous, hash)) = cache.get(path)
        && previous == &stamp
    {
        return hash.clone();
    }
    let mut hash = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(meta.mode().to_le_bytes());
    }
    let read = (|| -> std::io::Result<()> {
        if meta.file_type().is_symlink() {
            hash.update(b"symlink\0");
            hash.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
        } else {
            hash.update(b"file\0");
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options.open(path)?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
        }
        Ok(())
    })();
    if let Err(error) = read {
        hash.update(format!("unreadable:{:?}:{stamp:?}", error.kind()));
    }
    let stable = fs::symlink_metadata(path).is_ok_and(|after| Stamp::from(&after) == stamp);
    if !stable {
        // A moving file must not accidentally certify unchanged validation.
        hash.update(format!("changing:{:?}", SystemTime::now()));
    }
    let hash = format!("{:x}", hash.finalize());
    if stable {
        cache.insert(path.into(), (stamp, hash.clone()));
    }
    hash
}

pub fn tree(root: &Path, state: &Path) -> Result<String> {
    let root = fs::canonicalize(root).context("Workspace directory is unavailable")?;
    fs::create_dir_all(state)?;
    let state = fs::canonicalize(state)?;
    anyhow::ensure!(
        state != root,
        "Runtime state cannot replace the workspace directory"
    );
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut files = BTreeMap::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                files.insert(
                    dir.strip_prefix(&root)?.to_string_lossy().into_owned(),
                    format!("unreadable-directory:{:?}", error.kind()),
                );
                continue;
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if path.starts_with(&state)
                || name == ".git"
                || name == ".chuggin"
                || name.to_string_lossy().starts_with(".chuggin-save-")
                || (dir == root && name == "chuggin.json")
            {
                continue;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    files.insert(
                        path.strip_prefix(&root)?.to_string_lossy().into_owned(),
                        format!("unreadable:{:?}", error.kind()),
                    );
                    continue;
                }
            };
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() || meta.file_type().is_symlink() {
                files.insert(
                    path.strip_prefix(&root)?.to_string_lossy().into_owned(),
                    entry_hash(&path, &meta, &mut cache),
                );
            }
            // FIFOs, devices, and sockets are not opened or followed.
        }
    }
    cache.retain(|path, _| {
        !path.starts_with(&root)
            || path
                .strip_prefix(&root)
                .is_ok_and(|p| files.contains_key(p.to_string_lossy().as_ref()))
    });
    drop(cache);
    let manifest = Manifest { files };
    let bytes = serde_json::to_vec(&manifest)?;
    let id = format!("files-{:x}", Sha256::digest(&bytes));
    let path = manifest_path(&state, &id)?;
    if !path.exists() {
        crate::setup::save(&path, &manifest)?;
    }
    Ok(id)
}

fn manifest_path(state: &Path, id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        id.starts_with("files-")
            && id.len() == 70
            && id[6..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid file observation identifier"
    );
    Ok(state.join("file-observations").join(format!("{id}.json")))
}

pub fn changed_paths(state: &Path, before: &str, after: &str) -> Result<Vec<String>> {
    let after: Manifest = serde_json::from_slice(&fs::read(manifest_path(state, after)?)?)?;
    let before = manifest_path(state, before)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok());
    // Old Git observations or absent historical manifests never certify a file
    // unchanged. Re-read current paths when crossing the backend boundary.
    let Some(before) = before else {
        return Ok(after.files.into_keys().collect());
    };
    let paths: BTreeSet<_> = before.files.keys().chain(after.files.keys()).collect();
    Ok(paths
        .into_iter()
        .filter(|p| before.files.get(*p) != after.files.get(*p))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_handle_ignores_deletions_and_restarts_without_git() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let state = root.join("runtime");
        fs::create_dir(root.join("cache")).unwrap();
        fs::write(root.join("cache/item"), "first").unwrap();
        let before = tree(root, &state).unwrap();
        assert_eq!(tree(root, &state).unwrap(), before);
        fs::write(root.join(".gitignore"), "cache/\n").unwrap();
        fs::write(root.join("cache/item"), "other").unwrap();
        let after = tree(root, &state).unwrap();
        assert_eq!(
            changed_paths(&state, &before, &after).unwrap(),
            vec![".gitignore", "cache/item"]
        );
        fs::remove_file(root.join("cache/item")).unwrap();
        let deleted = tree(root, &state).unwrap();
        assert_eq!(
            changed_paths(&state, &after, &deleted).unwrap(),
            vec!["cache/item"]
        );
        assert!(!root.join(".git").exists());
        CACHE.lock().unwrap().clear();
        assert_eq!(tree(root, &state).unwrap(), deleted);
        assert!(
            changed_paths(&state, "legacy-git-tree", &deleted)
                .unwrap()
                .contains(&".gitignore".into())
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_special_files_and_git_metadata_do_not_escape_or_change_observations() {
        let d = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("secret"), "outside").unwrap();
        std::os::unix::fs::symlink(external.path(), d.path().join("link")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(d.path().join("socket")).unwrap();
        fs::create_dir(d.path().join(".git")).unwrap();
        let state = d.path().join(".chuggin");
        let first = tree(d.path(), &state).unwrap();
        fs::write(external.path().join("secret"), "changed outside").unwrap();
        fs::write(d.path().join(".git/HEAD"), "metadata").unwrap();
        assert_eq!(tree(d.path(), &state).unwrap(), first);
        fs::remove_file(d.path().join("link")).unwrap();
        std::os::unix::fs::symlink(external.path().join("secret"), d.path().join("link")).unwrap();
        let after = tree(d.path(), &state).unwrap();
        assert_eq!(changed_paths(&state, &first, &after).unwrap(), vec!["link"]);
    }
}
