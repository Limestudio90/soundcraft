//! The scan cache file: what each plugin file held when it was last loaded, keyed by the size and
//! modification time of its binary, so a scan loads only new and changed files (as REAPER does).
//! A file that failed to load is remembered too and not retried until it changes.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Bumped when the layout changes; a file of another version is ignored (everything is loaded).
const VERSION: u32 = 1;

/// Largest cache file read (a plugin takes a few hundred bytes).
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// One plugin file: the plugins it holds, or why it did not load.
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "D: Serialize", deserialize = "D: DeserializeOwned"))]
pub struct Entry<D> {
    pub path: PathBuf,
    size: u64,
    modified_ns: u64,
    pub plugins: Vec<D>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
#[serde(bound = "D: DeserializeOwned")]
struct FileIn<D> {
    version: u32,
    files: Vec<Entry<D>>,
}

#[derive(Serialize)]
#[serde(bound = "D: Serialize")]
struct FileOut<'a, D> {
    version: u32,
    files: &'a [Entry<D>],
}

/// Size and modification time (ns since 1970) of a file; `None` when unreadable.
fn stamp(file: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(file).ok()?;
    let ns = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some((m.len(), u64::try_from(ns).ok()?))
}

/// The entries of a cache file; none when it is missing, unreadable or of another version.
pub fn read<D: DeserializeOwned>(file: &Path) -> Vec<Entry<D>> {
    use std::io::Read as _;
    let Ok(f) = std::fs::File::open(file) else { return Vec::new() };
    let mut bytes = Vec::new();
    if f.take(MAX_BYTES).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    match serde_json::from_slice::<FileIn<D>>(&bytes) {
        Ok(c) if c.version == VERSION => c.files,
        Ok(_) => Vec::new(),
        Err(e) => {
            log::warn!("{}: {e}", file.display());
            Vec::new()
        }
    }
}

/// Writes a cache file (through a temporary file, so a crash never leaves half of one).
pub fn write<D: Serialize>(file: &Path, files: &[Entry<D>]) {
    let tmp = file.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(&FileOut { version: VERSION, files })
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&tmp, bytes).map_err(|e| e.to_string())
        })
        .and_then(|()| std::fs::rename(&tmp, file).map_err(|e| e.to_string()));
    if let Err(e) = written {
        log::warn!("{}: {e}", file.display());
    }
}

/// One entry per plugin file in `files`: the `known` entry when the file's `binary` (else the
/// file itself) has the same size and modification time, else what `load` reads now.
pub fn scan<D: Clone>(
    files: Vec<PathBuf>,
    known: &[Entry<D>],
    binary: impl Fn(&Path) -> Option<PathBuf>,
    load: impl Fn(&Path) -> Result<Vec<D>, String>,
) -> Vec<Entry<D>> {
    files
        .into_iter()
        .map(|path| {
            let stamp = stamp(&binary(&path).unwrap_or_else(|| path.clone()));
            if let Some((size, modified_ns)) = stamp
                && let Some(e) = known.iter().find(|e| e.path == path && e.size == size && e.modified_ns == modified_ns)
            {
                return e.clone();
            }
            let (plugins, error) = match load(&path) {
                Ok(p) => (p, None),
                Err(e) => {
                    log::warn!("{e}");
                    (Vec::new(), Some(e))
                }
            };
            let (size, modified_ns) = stamp.unwrap_or_default();
            Entry { path, size, modified_ns, plugins, error }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sc-scan-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn unchanged_files_are_not_loaded_again() {
        let d = dir("unchanged");
        let (a, b) = (d.join("a.plug"), d.join("b.plug"));
        std::fs::write(&a, "a").ok();
        std::fs::write(&b, "bb").ok();
        let loads = Cell::new(0);
        let load = |p: &Path| {
            loads.set(loads.get() + 1);
            if p == b { Err("b is broken".to_string()) } else { Ok(vec![p.display().to_string()]) }
        };
        let first = scan(vec![a.clone(), b.clone()], &[], |_| None, load);
        assert_eq!(loads.get(), 2);
        let file = d.join("cache.json");
        write(&file, &first);
        let known: Vec<Entry<String>> = read(&file);
        assert_eq!(known.len(), 2);

        // Both unchanged: nothing is loaded, the failure is remembered.
        let again = scan(vec![a.clone(), b.clone()], &known, |_| None, load);
        assert_eq!(loads.get(), 2);
        assert_eq!(again[0].plugins, vec![a.display().to_string()]);
        assert_eq!(again[1].error.as_deref(), Some("b is broken"));

        // A changed file and a new one are loaded; a removed one is dropped.
        std::fs::write(&b, "fixed, longer").ok();
        let c = d.join("c.plug");
        std::fs::write(&c, "c").ok();
        let next = scan(vec![b.clone(), c.clone()], &known, |_| None, load);
        assert_eq!(loads.get(), 4);
        assert_eq!(next.iter().map(|e| e.path.clone()).collect::<Vec<_>>(), vec![b, c]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_or_foreign_cache_file_is_ignored() {
        let d = dir("foreign");
        let file = d.join("cache.json");
        assert!(read::<String>(&file).is_empty());
        std::fs::write(&file, "not json").ok();
        assert!(read::<String>(&file).is_empty());
        std::fs::write(&file, r#"{"version":999,"files":[]}"#).ok();
        assert!(read::<String>(&file).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
