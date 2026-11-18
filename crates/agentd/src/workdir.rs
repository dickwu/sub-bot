//! Session directory selection; this policy is not a filesystem sandbox.
use crate::config::Profile;
use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) enum CwdError {
    Invalid(&'static str),
    NoBase(String),
    Denied(String),
    Mismatch,
}

impl fmt::Display for CwdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::NoBase(name) => write!(f, "profile '{name}' has no existing directory in workspaceRoots for a default or relative cwd"),
            Self::Denied(name) => write!(f, "cwd is not allowed in profile '{name}' (allowedWorkspaceRoots plus workspaceRoots)"),
            Self::Mismatch => f.write_str("cwd is fixed for this session; start a new session to use a different cwd"),
        }
    }
}

impl std::error::Error for CwdError {}

impl CwdError {
    pub(crate) fn decision(&self) -> &'static str {
        match self {
            Self::Invalid(_) | Self::NoBase(_) => "invalid",
            Self::Denied(_) => "denied",
            Self::Mismatch => "mismatch",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedCwd {
    pub cwd: PathBuf,
    pub session_root: PathBuf,
    pub decision: &'static str,
}

impl ResolvedCwd {
    pub(crate) fn restore_root(&mut self, meta: &agent_session::SessionMeta) -> Result<(), CwdError> {
        let saved_absolute_cwd = meta.cwd.as_deref().filter(|path| path.is_absolute());
        if saved_absolute_cwd.is_some_and(|saved| saved.as_os_str() != self.cwd.as_os_str()) {
            return Err(CwdError::Invalid("stored cwd is no longer canonical"));
        }
        if let Some(saved) = &meta.session_root {
            if saved_absolute_cwd.is_none() || !saved.is_absolute() || !self.cwd.starts_with(saved) || !canonical_dir(saved).is_ok_and(|root| root.as_os_str() == saved.as_os_str()) {
                return Err(CwdError::Invalid("invalid stored sessionRoot: expected a canonical directory containing cwd"));
            }
            if !saved.starts_with(&self.session_root) {
                return Err(CwdError::Invalid("stored sessionRoot exceeds the current context boundary"));
            }
            // A newly matching workspace must not broaden a saved context boundary.
            self.session_root = saved.clone();
        }
        Ok(())
    }
}

fn canonical_dir(path: &Path) -> io::Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    if !fs::metadata(&path)?.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(path)
}

fn cwd_io_error(err: io::Error) -> CwdError {
    CwdError::Invalid(match err.kind() {
        io::ErrorKind::NotFound => "cwd does not exist",
        io::ErrorKind::NotADirectory => "cwd is not a directory",
        _ => "cwd cannot be resolved (check path and permissions)",
    })
}

fn check_nonempty(path: &Path) -> Result<(), CwdError> {
    if path.as_os_str().is_empty() {
        Err(CwdError::Invalid("cwd must not be empty"))
    } else {
        Ok(())
    }
}

/// Interpret caller-local paths without changing the process's working directory.
pub(crate) fn cli_cwd(path: &Path) -> Result<PathBuf, CwdError> {
    check_nonempty(path)?;
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    let base = std::env::current_dir().map_err(|_| CwdError::Invalid("cwd cannot be resolved (check path and permissions)"))?;
    Ok(base.join(path))
}

/// Relative requests use the first usable workspace root. CLI callers first make
/// their paths absolute to use the launching process's cwd instead.
pub(crate) fn resolve_cwd(name: &str, profile: &Profile, requested: Option<&Path>) -> Result<ResolvedCwd, CwdError> {
    if let Some(path) = requested {
        check_nonempty(path)?;
    }
    let roots: Vec<_> = profile.workspace_roots.iter().filter_map(|r| canonical_dir(r).ok()).collect();
    let base = || roots.first().ok_or_else(|| CwdError::NoBase(name.to_owned()));
    let cwd = match requested {
        None => base()?.clone(),
        Some(path) if path.is_absolute() => canonical_dir(path).map_err(cwd_io_error)?,
        Some(path) => canonical_dir(&base()?.join(path)).map_err(cwd_io_error)?,
    };
    if cwd.to_str().is_none() {
        return Err(CwdError::Invalid("cwd must be representable as UTF-8"));
    }
    if let Some(root) = roots.into_iter().find(|root| cwd.starts_with(root)) {
        return Ok(ResolvedCwd { cwd, session_root: root, decision: "workspace_root" });
    }
    let decision = if profile.allows_any_workspace() {
        "any"
    } else if profile.allowed_workspace_roots.iter().filter_map(|r| canonical_dir(r).ok()).any(|root| cwd.starts_with(root)) {
        "allowed_workspace_root"
    } else {
        return Err(CwdError::Denied(name.to_owned()));
    };
    Ok(ResolvedCwd { session_root: cwd.clone(), cwd, decision })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions_tests::TempDir;

    fn mkdir(dir: &TempDir, name: &str) -> PathBuf {
        let path = dir.0.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn profile(roots: Vec<PathBuf>) -> Profile {
        serde_json::from_value(serde_json::json!({"workspaceRoots": roots})).unwrap()
    }

    #[test]
    fn cwd_any_allows_external_and_keeps_profile_default() {
        let dir = TempDir::new();
        let sub = mkdir(&dir, "workspace/sub");
        let workspace = sub.parent().unwrap().to_owned();
        let other = mkdir(&dir, "other");
        let file = dir.0.join("file");
        fs::write(&file, "").unwrap();
        let mut p = profile(vec![dir.0.join("missing"), file, workspace.clone(), sub.clone()]);
        for (request, cwd, root) in [(None, &workspace, &workspace), (Some(Path::new("sub")), &sub, &workspace), (Some(Path::new("../other")), &other, &other)] {
            let pair = resolve_cwd("p", &p, request).unwrap();
            assert_eq!((&pair.cwd, &pair.session_root), (cwd, root));
        }
        p.workspace_roots = vec![dir.0.join("missing")];
        assert_eq!(resolve_cwd("p", &p, Some(&other)).unwrap().session_root, other);
        p.workspace_roots = vec![sub.clone(), workspace];
        assert_eq!(resolve_cwd("p", &p, Some(&sub)).unwrap().session_root, sub);
    }

    #[test]
    fn cwd_list_unions_workspace_and_extra_roots() {
        let dir = TempDir::new();
        let sub = mkdir(&dir, "workspace/sub");
        let workspace = sub.parent().unwrap().to_owned();
        let job = mkdir(&dir, "extra/job");
        let extra = job.parent().unwrap().to_owned();
        let sibling = mkdir(&dir, "extra-similar");
        let mut p = profile(vec![workspace.clone()]);
        p.allowed_workspace_roots = vec![extra.clone()];
        for (path, root) in [(&workspace, &workspace), (&sub, &workspace), (&extra, &extra), (&job, &job)] {
            let pair = resolve_cwd("p", &p, Some(path)).unwrap();
            assert_eq!((&pair.cwd, &pair.session_root), (path, root));
        }
        assert!(matches!(resolve_cwd("p", &p, Some(&sibling)), Err(CwdError::Denied(_))));
        assert!(matches!(resolve_cwd("p", &p, Some(Path::new("../extra-similar"))), Err(CwdError::Denied(_))));
        for roots in [vec![], vec![dir.0.join("missing")]] {
            p.allowed_workspace_roots = roots;
            assert!(resolve_cwd("p", &p, Some(&sub)).is_ok());
            assert!(matches!(resolve_cwd("p", &p, Some(&job)), Err(CwdError::Denied(_))));
        }
        p.allowed_workspace_roots = vec![extra];
        p.workspace_roots = vec![dir.0.join("missing")];
        assert_eq!(resolve_cwd("p", &p, Some(&job)).unwrap().cwd, job);
    }

    #[test]
    fn cwd_rejects_missing_and_not_directory() {
        let dir = TempDir::new();
        let file = dir.0.join("file");
        fs::write(&file, "").unwrap();
        let mut p = profile(vec![dir.0.clone()]);
        for roots in [vec!["*".into()], vec![]] {
            p.allowed_workspace_roots = roots;
            for (path, message) in [("", "cwd must not be empty"), ("missing", "cwd does not exist"), ("file", "cwd is not a directory"), ("file/child", "cwd is not a directory")] {
                assert_eq!(resolve_cwd("p", &p, Some(Path::new(path))).unwrap_err().to_string(), message);
            }
            assert_eq!(resolve_cwd("p", &p, Some(Path::new("."))).unwrap().cwd, fs::canonicalize(&dir.0).unwrap());
        }
        p.workspace_roots = vec![file];
        for request in [None, Some(Path::new("."))] {
            assert!(matches!(resolve_cwd("p", &p, request), Err(CwdError::NoBase(_))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cwd_restricted_canonicalizes_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let workspace = mkdir(&dir, "workspace");
        let outside = mkdir(&dir, "outside");
        symlink(outside, workspace.join("escape")).unwrap();
        symlink(&workspace, dir.0.join("alias")).unwrap();
        symlink(dir.0.join("missing"), workspace.join("dangling")).unwrap();
        let mut p = profile(vec![workspace.clone()]);
        p.allowed_workspace_roots.clear();
        assert!(matches!(resolve_cwd("p", &p, Some(Path::new("escape"))), Err(CwdError::Denied(_))));
        assert_eq!(resolve_cwd("p", &p, Some(&dir.0.join("alias"))).unwrap().cwd, workspace);
        assert_eq!(resolve_cwd("p", &p, Some(Path::new("dangling"))).unwrap_err().to_string(), "cwd does not exist");
    }

    // macOS filesystems can refuse to create non-UTF-8 names in the first place.
    #[cfg(target_os = "linux")]
    #[test]
    fn cwd_rejects_non_utf8_after_canonicalization() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let dir = TempDir::new();
        let workspace = mkdir(&dir, "workspace");
        let p = profile(vec![workspace.clone()]);
        let raw = workspace.join(std::ffi::OsStr::from_bytes(b"\xff"));
        fs::create_dir(&raw).unwrap();
        symlink(raw, workspace.join("non-utf8")).unwrap();
        assert_eq!(resolve_cwd("p", &p, Some(Path::new("non-utf8"))).unwrap_err().to_string(), "cwd must be representable as UTF-8");
    }
}
