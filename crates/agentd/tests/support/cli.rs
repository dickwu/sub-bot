use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};
use tokio::process::Command;

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        // Keep paths short enough for macOS Unix socket addresses.
        let path = std::env::temp_dir().join(format!("sb-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    // env_clear alone can lead config discovery to the OS user's real home directory.
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("NO_COLOR", "1")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("XDG_DATA_HOME", dir.join("data-home"))
        .env("TMPDIR", dir)
        .current_dir(dir)
        .kill_on_drop(true);
    cmd
}

pub async fn output(cmd: &mut Command) -> Output {
    tokio::time::timeout(Duration::from_secs(15), cmd.output()).await.expect("sub-bot did not exit before the deadline").unwrap()
}
