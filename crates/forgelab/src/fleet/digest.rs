//! A hash of everything in a fleet that reaches a git tree.

use std::fs;
use std::io::Read;
use std::path::Path;

use sha2::{Digest as _, Sha256};

use super::FleetError;
use super::spec::{REPOS_DIR, SPEC_FILE};

/// Hashes fleet.yaml and everything under repos/. It is stamped into the lock so that a
/// fixture edited without re-running apply is caught before anything is reset against a
/// stale baseline.
///
/// The walk order is the one Go's `fs.WalkDir` uses: entries sorted by name within each
/// directory, depth first, so that the digest of an existing fleet is unchanged.
pub fn digest(root: &Path) -> Result<String, FleetError> {
    let mut h = Sha256::new();
    add(&mut h, root, SPEC_FILE, false)?;

    let walker = walkdir::WalkDir::new(root.join(REPOS_DIR))
        .follow_links(false)
        .sort_by_file_name();
    for entry in walker {
        let entry = entry.map_err(|e| {
            let path = e.path().map(|p| p.display().to_string()).unwrap_or_else(|| REPOS_DIR.to_string());
            match e.into_io_error() {
                Some(io) => FleetError::io(format!("read {path}"), io),
                None => FleetError::invalid(format!("read {path}: walk error")),
            }
        })?;
        if entry.file_type().is_dir() || is_os_junk(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| FleetError::invalid(format!("{}: outside the fleet", entry.path().display())))?;
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let exec = is_executable(&entry.path().metadata().map_err(|e| FleetError::io(format!("stat {rel}"), e))?);
        add(&mut h, root, &rel, exec)?;
    }
    Ok(format!("sha256:{}", hex::encode(h.finalize())))
}

fn add(h: &mut Sha256, root: &Path, rel: &str, exec: bool) -> Result<(), FleetError> {
    let mut f = fs::File::open(root.join(rel)).map_err(|e| FleetError::io(format!("open {rel}"), e))?;
    let size = f.metadata().map_err(|e| FleetError::io(format!("stat {rel}"), e))?.len();
    // Path, the executable bit and a length-delimited body: everything that reaches the git
    // tree, and nothing a checkout can change on its own.
    h.update(format!("{rel}\0{exec}\0{size}\0").as_bytes());
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| FleetError::io(format!("read {rel}"), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(())
}

/// The executable bit as git records it: any of the three x bits set.
pub fn is_executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Files that operating systems scatter through directories and that must never reach a
/// fixture commit, since their appearance would change a repository's SHA.
pub fn is_os_junk(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::spec::tests::fleet;

    const SPEC: &str = "version: 1\ngit:\n  author: {name: a, email: b}\n  timestamp: \"2026-01-01T00:00:00Z\"\n";

    #[test]
    fn tracks_content_but_not_junk() {
        let dir = fleet(SPEC, &[("repos/plain/README.md", "# plain\n")]);
        let before = digest(dir.path()).unwrap();
        std::fs::write(dir.path().join("repos/plain/.DS_Store"), "junk").unwrap();
        assert_eq!(digest(dir.path()).unwrap(), before, "OS junk changed the digest");
        std::fs::write(dir.path().join("repos/plain/README.md"), "# edited\n").unwrap();
        assert_ne!(digest(dir.path()).unwrap(), before, "an edited fixture did not change the digest");
    }

    /// Go's fs.WalkDir sorts entries by name within a directory: "a" the directory comes
    /// before "a.txt" the file, which a sort of full paths would reverse.
    #[test]
    fn walk_order_is_per_directory() {
        let dir = fleet(SPEC, &[("repos/r/a/x", "1"), ("repos/r/a.txt", "2")]);
        let mut h = Sha256::new();
        h.update(format!("{SPEC_FILE}\0false\0{}\0", SPEC.len()).as_bytes());
        h.update(SPEC.as_bytes());
        h.update(b"repos/r/a/x\0false\01\0");
        h.update(b"1");
        h.update(b"repos/r/a.txt\0false\01\0");
        h.update(b"2");
        assert_eq!(digest(dir.path()).unwrap(), format!("sha256:{}", hex::encode(h.finalize())));
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_is_hashed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fleet(SPEC, &[("repos/r/run.sh", "#!/bin/sh\n")]);
        let before = digest(dir.path()).unwrap();
        std::fs::set_permissions(dir.path().join("repos/r/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(digest(dir.path()).unwrap(), before);
    }
}
