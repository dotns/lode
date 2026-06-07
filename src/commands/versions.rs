//! `lode versions` — list locally installed versions, marking the active one.
//!
//! Purely local: enumerate the directories under `$DATA_DIR/versions/` (design
//! §15), resolve the `current` symlink to flag the active version, sort
//! semver-descending and print one per line. No network and no `state.json`
//! writes — read-only over the data dir.

use std::cmp::Ordering;
use std::io::Write;
use std::path::Path;

use crate::config::Config;
use crate::error::Result;

/// Print installed versions newest-first, marking the `current` one with `*`.
/// Writes through a locked stdout handle (the `println!` macro is denied
/// workspace-wide).
pub(crate) fn run(cfg: &Config) -> Result<()> {
    let data_dir = &cfg.global.data_dir;
    let versions = collect(data_dir)?;
    let current = current_version(data_dir);

    let mut out = std::io::stdout().lock();
    if versions.is_empty() {
        writeln!(out, "lode versions: none installed")?;
        return Ok(());
    }

    writeln!(out, "lode versions ({} installed)", versions.len())?;
    for v in &versions {
        let mark = if current.as_deref() == Some(v.as_str()) {
            '*'
        } else {
            ' '
        };
        writeln!(out, "{mark} {v}")?;
    }
    Ok(())
}

/// Directory names under `$DATA_DIR/versions/`, sorted semver-descending. An
/// absent `versions/` dir yields an empty list (nothing installed yet).
fn collect(data_dir: &Path) -> Result<Vec<String>> {
    let dir = data_dir.join("versions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            versions.push(name.to_owned());
        }
    }
    versions.sort_by(|a, b| cmp_desc(a, b));
    Ok(versions)
}

/// Resolve the `current` symlink to the version it points at, if any. Returns
/// `None` when the link is absent or not a symlink.
fn current_version(data_dir: &Path) -> Option<String> {
    let target = std::fs::read_link(data_dir.join("current")).ok()?;
    target
        .file_name()
        .and_then(|n| n.to_str())
        .map(ToOwned::to_owned)
}

/// Order two version names newest-first. Valid semver sorts by precedence
/// (descending) and ahead of any non-semver name; non-semver names fall back to
/// reverse lexicographic so the ordering stays total and deterministic.
fn cmp_desc(a: &str, b: &str) -> Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => y.cmp(&x),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => b.cmp(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty scratch data dir unique to this process + label.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lode-versions-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn absent_versions_dir_is_empty() {
        let dir = scratch("absent");
        assert!(collect(&dir).unwrap().is_empty());
        assert!(current_version(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_semver_descending_and_ignores_files() {
        let dir = scratch("list");
        let versions = dir.join("versions");
        for v in ["1.0.0", "1.5.0", "1.4.2"] {
            std::fs::create_dir_all(versions.join(v)).unwrap();
        }
        // A stray non-directory entry must be ignored.
        std::fs::write(versions.join("notes.txt"), b"ignore me").unwrap();

        assert_eq!(collect(&dir).unwrap(), ["1.5.0", "1.4.2", "1.0.0"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn current_symlink_resolves_to_version() {
        let dir = scratch("current");
        let versions = dir.join("versions");
        std::fs::create_dir_all(versions.join("1.5.0")).unwrap();
        std::os::unix::fs::symlink(versions.join("1.5.0"), dir.join("current")).unwrap();

        assert_eq!(current_version(&dir).as_deref(), Some("1.5.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
