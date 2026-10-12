//! The set of installed JSFX effects: scanned on first use from REAPER's Effects folder, the
//! `JSFX_PATH` folders and any added with [`add_search_dir`], then cached. Lookups by plugin id
//! (`jsfx:<path relative to its Effects folder>`) create instances from the files on disk.

use crate::scan::{JsfxEntry, default_effects_dirs, scan as scan_dir, soundcraft_effects_dir};
use crate::{ID_PREFIX, JsfxPlugin};
use soundcraft_dsp::{Plugin, PluginInfo};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

struct State {
    dirs: Vec<PathBuf>,
    /// Each effect with the Effects folder its id is relative to; `None` until the first scan.
    found: Option<Vec<(PathBuf, JsfxEntry)>>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    let m = STATE.get_or_init(|| {
        // SoundCraft's own folder comes first (it may not exist yet: installs create it), so what
        // was installed from a repository wins over a same-named file in REAPER's folder.
        let mut dirs: Vec<PathBuf> = soundcraft_effects_dir().into_iter().collect();
        dirs.extend(default_effects_dirs());
        if let Some(extra) = std::env::var_os("JSFX_PATH") {
            dirs.extend(std::env::split_paths(&extra).filter(|d| d.is_dir()));
        }
        Mutex::new(State { dirs, found: None })
    });
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn scanned(st: &mut State) -> &[(PathBuf, JsfxEntry)] {
    if st.found.is_none() {
        let mut all: Vec<(PathBuf, JsfxEntry)> = Vec::new();
        for root in &st.dirs {
            for e in scan_dir(root) {
                // An earlier folder wins when two hold the same relative path.
                if !all.iter().any(|(_, x)| x.id == e.id) {
                    all.push((root.clone(), e));
                }
            }
        }
        st.found = Some(all);
    }
    st.found.as_deref().unwrap_or(&[])
}

/// The JSFX id's path part (`None` for other ids or an empty path).
pub fn parse_id(id: &str) -> Option<&str> {
    id.strip_prefix(ID_PREFIX).filter(|s| !s.is_empty() && !s.contains('\0'))
}

/// Every installed JSFX effect, scanned on the first call and cached.
pub fn scan() -> Vec<JsfxEntry> {
    scanned(&mut state()).iter().map(|(_, e)| e.clone()).collect()
}

/// Forgets the cached scan and scans again.
pub fn rescan() -> Vec<JsfxEntry> {
    state().found = None;
    scan()
}

/// Adds an Effects folder to the search path. Returns how many effects the folder holds.
pub fn add_search_dir(dir: &Path) -> usize {
    let mut st = state();
    if !st.dirs.iter().any(|d| d == dir) {
        st.dirs.push(dir.to_path_buf());
        st.found = None;
    }
    scan_dir(dir).len()
}

/// Creates an effect by `jsfx:<path>`, prepared for 48 kHz stereo with 1024-frame blocks (like
/// `soundcraft_dsp::create`); `None` if unknown or it fails to load.
pub fn create(id: &str) -> Option<Box<dyn Plugin>> {
    parse_id(id)?;
    let (root, path) = {
        let mut st = state();
        scanned(&mut st).iter().find(|(_, e)| e.id == id).map(|(r, e)| (r.clone(), e.path.clone()))?
    };
    match JsfxPlugin::from_file(&root, &path) {
        Ok(mut p) => {
            p.prepare(48_000.0, 1024, 2);
            Some(Box::new(p))
        }
        Err(e) => {
            log::warn!("cannot load {id}: {e}");
            None
        }
    }
}

/// The description of a `jsfx:<path>` effect. The first call for an effect compiles it once to
/// read its sliders; the result is cached (and leaked, see the crate docs).
pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    create(id).map(|p| p.info())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse() {
        assert_eq!(parse_id("jsfx:dynamics/comp"), Some("dynamics/comp"));
        assert_eq!(parse_id("jsfx:"), None);
        assert_eq!(parse_id("clap:x"), None);
        assert_eq!(parse_id("jsfx:a\0b"), None);
    }

    #[test]
    fn non_jsfx_and_unknown_ids_are_rejected() {
        assert!(create("eq_7band").is_none());
        assert!(create("jsfx:no/such/effect").is_none());
        assert!(plugin_info("clap:x").is_none());
    }

    #[test]
    fn an_added_folder_is_scanned_and_creates_working_plugins() {
        let root = std::env::temp_dir().join(format!("sc-jsfx-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("util")).unwrap();
        std::fs::write(root.join("util/half"), "desc:Half\n@sample\nspl0*=0.5;spl1*=0.5;\n").unwrap();
        assert_eq!(add_search_dir(&root), 1);
        let id = "jsfx:util/half";
        assert!(scan().iter().any(|e| e.id == id && e.vendor == "util"));
        let info = plugin_info(id).unwrap();
        assert_eq!(info.name, "Half");
        let mut p = create(id).unwrap();
        let mut io = vec![vec![1.0f32; 4], vec![-1.0f32; 4]];
        p.process(&mut io, 4);
        assert_eq!((io[0][0], io[1][3]), (0.5, -0.5));
        let _ = std::fs::remove_dir_all(&root);
    }
}
