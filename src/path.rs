use crate::prelude::*;

const GIT_MARKER: &str = ".git";

const SCM_MARKERS: [&str; 6] = [
    GIT_MARKER,
    ".hg",
    ".svn",
    "_darcs",
    ".bzr",
    ".fossil-settings",
];

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".hgignore", ".ignore"];

/// Represents the project path, encapsulating repo and cwd paths, enough to resolve any crate
/// operation. It guarantees that the cwd and repo paths are absolute dir paths, as well as
/// presence of the repo path.
#[derive(Debug, Clone)]
pub struct ProjectPath {
    cwd_path: AbsDirPath,
    repo_path: RepoPath,
}

impl ProjectPath {
    /// Finds the project path by searching for the repo root starting from the given cwd.
    pub fn find<P: AsRef<Path>>(cwd: P) -> Result<Self> {
        let cwd_path = AbsDirPath {
            abs_path: AbsPath::try_new(cwd)?,
        };
        let repo_path = RepoPath::find(&cwd_path)?;
        Ok(ProjectPath {
            cwd_path,
            repo_path,
        })
    }

    /// Returns the cwd path.
    pub fn cwd_path(&self) -> &AbsDirPath {
        &self.cwd_path
    }

    /// Returns the repo path.
    pub fn repo_path(&self) -> &AbsDirPath {
        self.repo_path.abs_dir_path()
    }

    /// Checks if the given path is a project file, meaning it is a file within the cwd, not within
    /// a SCM dir, and exists as a file.
    pub fn is_project_file(&self, path: &AbsPath) -> bool {
        let is_within_cwd = self.cwd_path.is_or_contains(path);
        if !is_within_cwd || path.is_scm_path() {
            return false;
        }

        path.path_buf().is_file()
    }

    /// Returns the dirs path of the given path relative to the project cwd.
    pub fn dir_segments_to(&self, path: &AbsPath) -> Vec<AbsDirPath> {
        if !self.cwd_path.is_or_contains(path) {
            return vec![];
        }

        let mut segments = vec![];
        let mut current = path.parent();
        while let Some(dir_path) = current {
            if self.cwd_path.is_or_contains(&dir_path) {
                segments.push(dir_path.clone());
            } else {
                break;
            }

            if dir_path == self.cwd_path {
                break;
            }

            current = dir_path.parent();
        }

        segments.reverse();
        segments
    }

    /// Returns an absolute path within the project cwd.
    pub fn abs_path_within_cwd<P: AsRef<Path>>(&self, path: P) -> Option<AbsPath> {
        let abs_path = AbsPath::new(&self.cwd_path, path);
        if abs_path.is_or_within(&self.cwd_path()) {
            Some(abs_path)
        } else {
            None
        }
    }

    /// Returns an absolute path within the project git root.
    pub fn abs_path_within_repo<P: AsRef<Path>>(&self, path: P) -> Option<AbsPath> {
        // NOTE: We need to use cwd path for resolving relative paths, even though we are checking
        // against the repo path.
        let abs_path = AbsPath::new(&self.cwd_path(), path);
        if abs_path.is_or_within(&self.repo_path.abs_dir_path()) {
            Some(abs_path)
        } else {
            None
        }
    }
}

/// Represents the repo path, encapsulating the absolute dir path of the repo root. It guarantees
/// that the repo path is an existing absolute dir path with a `.git` marker.
#[derive(Debug, Clone)]
pub struct RepoPath {
    abs_dir_path: AbsDirPath,
}

impl RepoPath {
    /// Finds the repo path by searching for the `.git` marker starting from the given cwd and
    /// moving upwards until the marker is found or the root is reached.
    fn find(cwd_path: &AbsDirPath) -> Result<RepoPath> {
        let mut current = Some(cwd_path.abs_path.path_buf.as_path());
        while let Some(dir) = current {
            let git_marker = dir.join(GIT_MARKER);
            if git_marker.exists() {
                let path_buf = dir.to_path_buf();
                let abs_path = AbsPath { path_buf };
                let root_path = AbsDirPath { abs_path };
                return Ok(RepoPath {
                    abs_dir_path: root_path,
                });
            }
            current = dir.parent();
        }
        Err(anyhow::anyhow!("failed to find repo root"))
    }

    /// Returns the absolute dir path of the repo root.
    fn abs_dir_path(&self) -> &AbsDirPath {
        &self.abs_dir_path
    }
}

/// Represents the absolute path, guaranteeing that the path is an existing absolute dir path.
#[derive(Debug, Clone)]
pub struct AbsDirPath {
    abs_path: AbsPath,
}

impl AbsDirPath {
    /// Returns the path buf as a reference.
    pub fn path_buf(&self) -> &PathBuf {
        self.abs_path.path_buf()
    }

    /// Returns the path buf as a reference.
    pub fn path(&self) -> &Path {
        self.abs_path.path()
    }

    /// Returns the absolute path as a `PathBuf`.
    pub fn to_path_buf(&self) -> PathBuf {
        self.abs_path.path_buf.clone()
    }

    /// Return the absolute path as an `AbsPath`.
    pub fn to_abs_path(&self) -> AbsPath {
        self.abs_path.clone()
    }

    /// Checks if the given path is or within the dir path.
    pub fn is_or_contains<P: AsRef<AbsPath>>(&self, path: P) -> bool {
        let path = path.as_ref();
        path.is_or_within(&self)
    }

    /// Returns the parent dir path of this absolute path if it exists.
    pub fn parent(&self) -> Option<AbsDirPath> {
        self.abs_path.parent()
    }
}

impl AsRef<AbsPath> for AbsDirPath {
    fn as_ref(&self) -> &AbsPath {
        &self.abs_path
    }
}

impl Hash for AbsDirPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.abs_path.hash(state);
    }
}

impl PartialEq for AbsDirPath {
    fn eq(&self, other: &Self) -> bool {
        self.abs_path == other.abs_path
    }
}

impl Eq for AbsDirPath {}

/// Represents the absolute path, guaranteeing that the path is an absolute file path. It does not
/// check if the path exists.
#[derive(Debug, Clone, PartialOrd, Ord)]
pub struct AbsPath {
    path_buf: PathBuf,
}

impl AbsPath {
    /// Tries to resolve the given path to an absolute path. If the given path is not absolute,
    /// it is resolve relative to the current dir.
    pub fn try_new<P: AsRef<Path>>(path_src: P) -> Result<Self> {
        let path_src = path_src.as_ref();
        let path_buf = if path_src.is_absolute() {
            path_src.to_path_buf()
        } else {
            std::env::current_dir()
                .context("failed to resolve current dir")?
                .join(path_src)
        };
        Ok(Self { path_buf })
    }

    /// Creates a new absolute path by joining the given path to the base dir. If the given path is
    /// already absolute, it is returned as is.
    pub fn new<P: AsRef<Path>>(base: &AbsDirPath, path: P) -> Self {
        let path = path.as_ref();
        let path_buf = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.to_path_buf().join(path)
        };
        AbsPath { path_buf }
    }

    /// Checks if the path is or within the given dir path.
    pub fn is_or_within(&self, dir: &AbsDirPath) -> bool {
        self.path_buf.starts_with(&dir.abs_path.path_buf)
    }

    /// Returns the parent dir path of this absolute path if it exists.
    pub fn parent(&self) -> Option<AbsDirPath> {
        self.path_buf.parent().map(|parent_buf| AbsDirPath {
            abs_path: AbsPath {
                path_buf: parent_buf.to_path_buf(),
            },
        })
    }

    /// Returns the path buf as a reference.
    pub fn path_buf(&self) -> &PathBuf {
        &self.path_buf
    }

    /// Returns the path buf as a reference.
    pub fn path(&self) -> &Path {
        self.path_buf.as_path()
    }

    /// Checks if the path is or within a SCM dir.
    pub fn is_scm_path(&self) -> bool {
        self.path_buf.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|part| SCM_MARKERS.iter().any(|marker| *marker == part))
        })
    }

    /// Checks if the path is ignore-related.
    pub fn is_ignore_related(&self) -> bool {
        self.is_ignore_file() || self.is_git_exclude_file() || self.is_git_config_file()
    }

    /// Checks if the path is an ignore file.
    pub fn is_ignore_file(&self) -> bool {
        self.path_buf.file_name().is_some_and(|name| {
            name.to_str().is_some_and(|name| {
                IGNORE_FILE_NAMES
                    .iter()
                    .any(|ignore_name| *ignore_name == name)
            })
        })
    }

    /// Checks if the path is a Git exclude file.
    pub fn is_git_exclude_file(&self) -> bool {
        self.path_buf.to_string_lossy().ends_with(&format!(
            "{0}.git{0}info{0}exclude",
            std::path::MAIN_SEPARATOR
        ))
    }

    /// Checks if the path is a git config file.
    pub fn is_git_config_file(&self) -> bool {
        self.path_buf
            .to_string_lossy()
            .ends_with(&format!("{0}.git{0}config", std::path::MAIN_SEPARATOR))
    }

    #[cfg(feature = "source-filter")]
    /// Returns the source language of the path if it can be determined.
    pub fn source_lang(&self) -> Option<SourceLang> {
        SourceLang::from_path(&self.path_buf, &Default::default())
    }

    #[cfg(feature = "source-filter")]
    /// Checks if the path is a source file based on its extension.
    pub fn is_source_file(&self) -> bool {
        self.source_lang().is_some()
    }

    #[cfg(not(feature = "source-filter"))]
    /// Checks if the path is a source file based on its extension.
    pub fn is_source_file(&self) -> bool {
        false
    }

    /// Checks if the path matches the given gitignore glob pattern.
    pub fn matches_glob(&self, glob: &Glob) -> bool {
        glob.from()
            .is_none_or(|path| self.path_buf.starts_with(path))
    }
}

impl AsRef<AbsPath> for AbsPath {
    fn as_ref(&self) -> &AbsPath {
        self
    }
}

impl Hash for AbsPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path_buf.hash(state);
    }
}

impl PartialEq for AbsPath {
    fn eq(&self, other: &Self) -> bool {
        self.path_buf == other.path_buf
    }
}

impl Eq for AbsPath {}
