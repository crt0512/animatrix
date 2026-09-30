//! Bundled GIFs, installed to `<data dir>/animatrix/gifs`, and the fallback
//! lookup that keeps GIF elements working when their file is missing.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Shown by new GIF elements and whenever a GIF's own file cannot be found.
pub const DEFAULT_GIF: &str = "gaming/UFO.gif";

/// Folders holding the bundled GIFs, most specific first: one per XDG data
/// directory (normally `/usr/local/share` and `/usr/share`), and `data/gifs`
/// in the source tree for development builds.
pub fn gif_dirs() -> Vec<PathBuf> {
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&data_dirs)
        .map(|dir| dir.join("animatrix/gifs"))
        .collect();
    if cfg!(debug_assertions) {
        dirs.insert(0, Path::new(env!("CARGO_MANIFEST_DIR")).join("data/gifs"));
    }
    dirs.retain(|dir| dir.is_dir());
    dirs
}

/// The file a GIF element should play: `path` itself when it exists, else
/// `path` relative to a bundled GIF folder, else a bundled GIF with the same
/// file name, else [`DEFAULT_GIF`]. `None` only if nothing is installed.
pub fn resolve_gif(path: &Path) -> Option<PathBuf> {
    resolve_in(path, &gif_dirs())
}

fn resolve_in(path: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    if !path.as_os_str().is_empty() && path.is_file() {
        return Some(path.to_path_buf());
    }
    let relative = (!path.as_os_str().is_empty() && path.is_relative())
        .then(|| dirs.iter().map(|dir| dir.join(path)).find(|candidate| candidate.is_file()))
        .flatten();
    let by_name = || path.file_name().and_then(|name| dirs.iter().find_map(|dir| find_named(dir, name)));
    let found = relative.or_else(by_name).or_else(|| {
        dirs.iter().map(|dir| dir.join(DEFAULT_GIF)).find(|candidate| candidate.is_file())
    })?;
    warn_once(path, &found);
    Some(found)
}

/// Depth-first search for a file called `name` below `dir`.
fn find_named(dir: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(dir).ok()?.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    entries.iter().find(|entry| entry.is_file() && entry.file_name() == Some(name)).cloned()
        .or_else(|| entries.iter().filter(|entry| entry.is_dir()).find_map(|entry| find_named(entry, name)))
}

/// Logs a substitution once per path instead of on every frame.
fn warn_once(requested: &Path, used: &Path) {
    static WARNED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    if let Ok(mut warned) = WARNED.lock() {
        if warned.get_or_insert_with(HashSet::new).insert(requested.to_path_buf()) {
            eprintln!("animatrix: GIF '{}' not found, using '{}'", requested.display(), used.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn falls_back_through_relative_name_and_default() {
        let root = tempdir().unwrap();
        let dir = root.path().to_path_buf();
        for file in [DEFAULT_GIF, "music/DJ.gif"] {
            fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            fs::write(dir.join(file), b"GIF89a").unwrap();
        }
        let own = root.path().join("mine.gif");
        fs::write(&own, b"GIF89a").unwrap();
        let dirs = [dir.clone()];

        assert_eq!(resolve_in(&own, &dirs), Some(own.clone()));
        assert_eq!(resolve_in(Path::new("music/DJ.gif"), &dirs), Some(dir.join("music/DJ.gif")));
        assert_eq!(resolve_in(Path::new("/gone/DJ.gif"), &dirs), Some(dir.join("music/DJ.gif")));
        assert_eq!(resolve_in(Path::new(""), &dirs), Some(dir.join(DEFAULT_GIF)));
        assert_eq!(resolve_in(Path::new("/gone/other.gif"), &dirs), Some(dir.join(DEFAULT_GIF)));
        assert_eq!(resolve_in(Path::new("/gone/other.gif"), &[]), None);
    }
}
