//! Virtual filesystem seam for panel listings: local directories and (later)
//! remote ssh hosts both list through [`Vfs`], so the background [`Loader`]
//! does not care where a panel lives.
//!
//! [`Loader`]: crate::loader::Loader

use std::path::Path;
use std::sync::Arc;

use crate::loader::Listing;
use crate::session::{RemoteSession, SshVfs};

/// Lists directories. Implementations block, so they run on loader workers.
pub trait Vfs: Send + Sync {
    /// Entries (with `..` first) and the number of skipped items, or an error.
    fn list(&self, dir: &Path, show_hidden: bool) -> Listing;
    /// Human-readable location of `dir` (panel title).
    fn display_path(&self, dir: &Path) -> String;
    /// Identity of the backing store: 0 is the local filesystem, anything else
    /// a remote session. The loader abandons a slow read when a panel moves to
    /// a different source.
    fn source_key(&self) -> usize {
        0
    }
}

/// The local filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalVfs;

impl Vfs for LocalVfs {
    fn list(&self, dir: &Path, show_hidden: bool) -> Listing {
        crate::panel::read_listing(dir, show_hidden)
    }
    fn display_path(&self, dir: &Path) -> String {
        dir.to_string_lossy().into_owned()
    }
}

/// Where a panel's directory lives.
#[derive(Debug, Clone, Default)]
pub enum Location {
    #[default]
    Local,
    Remote(Arc<RemoteSession>),
}

impl Location {
    pub fn vfs(&self) -> Arc<dyn Vfs> {
        match self {
            Location::Local => Arc::new(LocalVfs),
            Location::Remote(s) => Arc::new(SshVfs {
                session: Arc::clone(s),
            }),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Location::Remote(_))
    }

    pub fn session(&self) -> Option<&Arc<RemoteSession>> {
        match self {
            Location::Remote(s) => Some(s),
            Location::Local => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_vfs_lists_like_read_listing() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(t.path().join("sub")).unwrap();
        std::fs::write(t.path().join(".h"), "x").unwrap();
        let (got, skipped) = LocalVfs.list(t.path(), false).unwrap();
        let (want, _) = crate::panel::read_listing(t.path(), false).unwrap();
        let names =
            |v: &[crate::panel::Entry]| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&got), names(&want));
        assert_eq!(names(&got), vec!["..", "sub", "a.txt"]);
        assert_eq!(skipped, 0);
        let (hidden, _) = LocalVfs.list(t.path(), true).unwrap();
        assert!(hidden.iter().any(|e| e.name == ".h"));
    }

    #[test]
    fn local_vfs_missing_dir_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(LocalVfs.list(&t.path().join("nope"), false).is_err());
    }

    #[test]
    fn local_display_path_is_the_plain_path() {
        assert_eq!(
            LocalVfs.display_path(Path::new("/tmp/한글 dir")),
            "/tmp/한글 dir"
        );
    }

    #[test]
    fn remote_location_lists_through_ssh() {
        use crate::remote::{RemoteOs, ScpMode, SshTarget};
        use crate::session::SystemCmd;
        let s = Arc::new(RemoteSession::new(
            "web".into(),
            SshTarget {
                host: "h".into(),
                port: None,
                control_path: "/tmp/x".into(),
            },
            RemoteOs::Linux,
            ScpMode::Sftp,
            Arc::new(SystemCmd),
        ));
        let loc = Location::Remote(s);
        assert!(loc.is_remote() && loc.session().is_some());
        assert_eq!(loc.vfs().display_path(Path::new("/a")), "web:/a");
    }

    #[test]
    fn default_location_is_local() {
        let loc = Location::default();
        assert!(!loc.is_remote());
        let t = tempfile::tempdir().unwrap();
        assert!(loc.vfs().list(t.path(), false).is_ok());
    }
}
