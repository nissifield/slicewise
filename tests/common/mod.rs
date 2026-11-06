use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// The user manager's unit directory: its own `$XDG_CONFIG_HOME`, not this
/// process's (a harness may set a different one, and systemd would then create
/// implicit slices with none of the rendered properties).
pub fn user_unit_dir() -> PathBuf {
    let manager = slicewise::systemd::systemctl(&["--user", "show-environment"])
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            line.strip_prefix("XDG_CONFIG_HOME=")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        });
    manager
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME unset")).join(".config")
        })
        .join("systemd/user")
}

/// The 15-line std-only tempdir helper (design §10, §11); no dev-dependencies.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .or_else(|| option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from))
            .unwrap_or_else(std::env::temp_dir);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
