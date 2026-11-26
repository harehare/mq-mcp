use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// Default upper bound for a file read through the `path` parameter.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// How deep `list` descends below the starting directory.
const MAX_LIST_DEPTH: usize = 32;

/// Directories `list` never descends into, besides dot-directories.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target"];

/// Read access to files, confined to a set of allowed root directories.
///
/// The roots come from `--root` when given. Otherwise they are the workspace
/// roots the MCP client shares (`roots/list`). With neither, file access is
/// disabled and every path is rejected.
#[derive(Debug, Clone)]
pub struct FileAccess {
    /// Roots set on the command line; they override whatever the client shares.
    roots: Vec<PathBuf>,
    /// Roots the client shared for this session.
    client_roots: Arc<RwLock<Vec<PathBuf>>>,
    max_file_size: u64,
}

impl Default for FileAccess {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            client_roots: Arc::default(),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

impl FileAccess {
    /// Creates a `FileAccess` limited to `roots`, which must all be existing directories.
    pub fn new(
        roots: impl IntoIterator<Item = PathBuf>,
        max_file_size: u64,
    ) -> Result<Self, String> {
        let roots = roots
            .into_iter()
            .map(|root| {
                let canonical = root
                    .canonicalize()
                    .map_err(|e| format!("Cannot use '{}' as a root: {e}", root.display()))?;
                if canonical.is_dir() {
                    Ok(canonical)
                } else {
                    Err(format!(
                        "Cannot use '{}' as a root: not a directory",
                        root.display()
                    ))
                }
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            roots,
            client_roots: Arc::default(),
            max_file_size,
        })
    }

    /// A copy with its own, empty set of client roots. Each MCP session must
    /// use one, so a client's workspace never leaks into another's.
    pub fn for_session(&self) -> Self {
        Self {
            client_roots: Arc::default(),
            ..self.clone()
        }
    }

    /// Whether `--root` was given, which makes client roots irrelevant.
    pub fn has_configured_roots(&self) -> bool {
        !self.roots.is_empty()
    }

    /// Replaces the roots shared by the client with `uris` (`file://` URIs).
    /// URIs that aren't local existing directories are skipped.
    pub fn set_client_roots<S: AsRef<str>>(&self, uris: impl IntoIterator<Item = S>) {
        let roots = uris
            .into_iter()
            .filter_map(|uri| file_uri_to_path(uri.as_ref()))
            .filter_map(|path| path.canonicalize().ok())
            .filter(|path| path.is_dir())
            .collect();
        *self
            .client_roots
            .write()
            .expect("client roots lock poisoned") = roots;
    }

    pub fn is_enabled(&self) -> bool {
        !self.roots().is_empty()
    }

    /// The directories that may be read: `--root` if given, else the client's.
    pub fn roots(&self) -> Vec<PathBuf> {
        if self.has_configured_roots() {
            self.roots.clone()
        } else {
            self.client_roots
                .read()
                .expect("client roots lock poisoned")
                .clone()
        }
    }

    fn disabled_message() -> String {
        "File access is disabled on this server: the client shared no workspace roots and no --root was given. Pass the content inline instead, or start the server with --root <dir> to allow reading files."
            .to_string()
    }

    fn denied_message(&self, path: &str) -> String {
        // Deliberately the same for "missing" and "outside the roots", so a path
        // outside the roots can't be probed for existence.
        let roots = self
            .roots()
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Cannot read '{path}': it does not exist or is outside the allowed directories ({roots})."
        )
    }

    /// Resolves `path` to a canonical path inside one of the roots.
    ///
    /// Relative paths are tried against each root in order. Symlinks and `..`
    /// are resolved before the containment check, so neither can escape a root.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        if !self.is_enabled() {
            return Err(Self::disabled_message());
        }

        let roots = self.roots();
        let path_ref = Path::new(path);
        let candidates: Vec<PathBuf> = if path_ref.is_absolute() {
            vec![path_ref.to_path_buf()]
        } else {
            roots.iter().map(|root| root.join(path_ref)).collect()
        };

        candidates
            .into_iter()
            .filter_map(|candidate| candidate.canonicalize().ok())
            .find(|real| roots.iter().any(|root| real.starts_with(root)))
            .ok_or_else(|| self.denied_message(path))
    }

    /// Reads a UTF-8 text file inside the allowed roots.
    pub fn read(&self, path: &str) -> Result<String, String> {
        let real = self.resolve(path)?;
        let metadata = fs::metadata(&real).map_err(|e| format!("Cannot read '{path}': {e}"))?;

        if !metadata.is_file() {
            return Err(format!("'{path}' is not a regular file."));
        }
        if metadata.len() > self.max_file_size {
            return Err(format!(
                "'{path}' is {} bytes, over the {} byte limit.",
                metadata.len(),
                self.max_file_size
            ));
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        fs::File::open(&real)
            .and_then(|file| file.take(self.max_file_size + 1).read_to_end(&mut bytes))
            .map_err(|e| format!("Cannot read '{path}': {e}"))?;

        if bytes.len() as u64 > self.max_file_size {
            return Err(format!(
                "'{path}' is over the {} byte limit.",
                self.max_file_size
            ));
        }

        String::from_utf8(bytes).map_err(|_| format!("'{path}' is not valid UTF-8 text."))
    }

    /// Lists files with one of `extensions` (case-insensitive, without the dot)
    /// below `directory`, or below every root when `directory` is `None`.
    ///
    /// Returns at most `limit` entries, sorted by path, and whether more exist.
    /// Symlinks, dot-directories, `node_modules` and `target` are skipped.
    pub fn list(
        &self,
        directory: Option<&str>,
        extensions: &[String],
        limit: usize,
    ) -> Result<(Vec<(PathBuf, u64)>, bool), String> {
        let starts = match directory {
            Some(dir) => {
                let real = self.resolve(dir)?;
                if !real.is_dir() {
                    return Err(format!("'{dir}' is not a directory."));
                }
                vec![real]
            }
            None if self.is_enabled() => self.roots(),
            None => return Err(Self::disabled_message()),
        };

        let mut files = Vec::new();
        let mut truncated = false;
        for start in starts {
            if !collect(&start, 0, extensions, limit, &mut files) {
                truncated = true;
                break;
            }
        }

        Ok((files, truncated))
    }
}

/// Converts a local `file://` URI to a path; `None` for other schemes or remote hosts.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let (host, path) = rest.split_at(rest.find('/')?);
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }

    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = std::str::from_utf8(raw.get(i + 1..i + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(bytes).ok()?;

    // `file:///C:/dir` carries the drive letter after the leading slash.
    #[cfg(windows)]
    let path = match path.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => path[1..].to_string(),
        _ => path,
    };

    Some(PathBuf::from(path))
}

/// Depth-first walk in name order. Returns `false` once `limit` is exceeded.
fn collect(
    dir: &Path,
    depth: usize,
    extensions: &[String],
    limit: usize,
    out: &mut Vec<(PathBuf, u64)>,
) -> bool {
    if depth > MAX_LIST_DEPTH {
        return true;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return true;
    };

    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `file_type` doesn't follow symlinks, which keeps the walk inside the root.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            if !collect(&entry.path(), depth + 1, extensions, limit, out) {
                return false;
            }
        } else if file_type.is_file() && has_extension(&entry.path(), extensions) {
            if out.len() >= limit {
                return false;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or_default();
            out.push((entry.path(), size));
        }
    }

    true
}

fn has_extension(path: &Path, extensions: &[String]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            extensions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tempfile::TempDir;

    /// `root/` is the allowed directory; `outside/` is a sibling that must stay unreachable.
    fn fixture() -> (TempDir, FileAccess) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("docs/nested")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::create_dir_all(tmp.path().join("outside")).unwrap();
        fs::write(root.join("a.md"), "# A").unwrap();
        fs::write(root.join("docs/b.md"), "# B").unwrap();
        fs::write(root.join("docs/nested/c.MD"), "# C").unwrap();
        fs::write(root.join("docs/notes.txt"), "text").unwrap();
        fs::write(root.join(".hidden/d.md"), "# D").unwrap();
        fs::write(tmp.path().join("outside/secret.md"), "# secret").unwrap();

        let access = FileAccess::new([root], 1024).unwrap();
        (tmp, access)
    }

    #[test]
    fn test_disabled_without_roots() {
        let err = FileAccess::default().read("a.md").unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    #[rstest]
    #[case::relative("a.md")]
    #[case::relative_nested("docs/b.md")]
    #[case::dot_segments("docs/../a.md")]
    fn test_read_inside_root(#[case] path: &str) {
        let (_tmp, access) = fixture();
        assert!(access.read(path).unwrap().starts_with('#'));
    }

    #[test]
    fn test_read_absolute_path_inside_root() {
        let (tmp, access) = fixture();
        let path = tmp.path().join("root/a.md");
        assert_eq!(access.read(path.to_str().unwrap()).unwrap(), "# A");
    }

    #[rstest]
    #[case::parent_traversal("../outside/secret.md")]
    #[case::deep_traversal("docs/../../outside/secret.md")]
    fn test_read_rejects_traversal(#[case] path: &str) {
        let (_tmp, access) = fixture();
        let err = access.read(path).unwrap_err();
        assert!(err.contains("outside the allowed directories"), "{err}");
    }

    #[test]
    fn test_read_rejects_absolute_path_outside_root() {
        let (tmp, access) = fixture();
        let path = tmp.path().join("outside/secret.md");
        assert!(access.read(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn test_missing_and_outside_errors_are_indistinguishable() {
        let (tmp, access) = fixture();
        let outside_existing = tmp.path().join("outside/secret.md");
        let outside_missing = tmp.path().join("outside/nope.md");
        let a = access.read(outside_existing.to_str().unwrap()).unwrap_err();
        let b = access.read(outside_missing.to_str().unwrap()).unwrap_err();
        assert_eq!(
            a.replace(outside_existing.to_str().unwrap(), "X"),
            b.replace(outside_missing.to_str().unwrap(), "X")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_read_rejects_symlink_escaping_root() {
        let (tmp, access) = fixture();
        std::os::unix::fs::symlink(
            tmp.path().join("outside/secret.md"),
            tmp.path().join("root/link.md"),
        )
        .unwrap();
        let err = access.read("link.md").unwrap_err();
        assert!(err.contains("outside the allowed directories"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn test_read_allows_symlink_staying_inside_root() {
        let (tmp, access) = fixture();
        std::os::unix::fs::symlink(
            tmp.path().join("root/a.md"),
            tmp.path().join("root/alias.md"),
        )
        .unwrap();
        assert_eq!(access.read("alias.md").unwrap(), "# A");
    }

    #[test]
    fn test_read_rejects_directory_and_oversize_and_non_utf8() {
        let (tmp, access) = fixture();
        assert!(
            access
                .read("docs")
                .unwrap_err()
                .contains("not a regular file")
        );

        fs::write(tmp.path().join("root/big.md"), "x".repeat(2048)).unwrap();
        assert!(access.read("big.md").unwrap_err().contains("limit"));

        fs::write(tmp.path().join("root/bin.md"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(access.read("bin.md").unwrap_err().contains("UTF-8"));
    }

    #[rstest]
    #[case::plain("file:///tmp/a", Some("/tmp/a"))]
    #[case::localhost("file://localhost/tmp/a", Some("/tmp/a"))]
    #[case::percent("file:///tmp/my%20docs", Some("/tmp/my docs"))]
    #[case::unicode("file:///tmp/%E3%83%A1%E3%83%A2", Some("/tmp/メモ"))]
    #[case::remote_host("file://example.com/tmp/a", None)]
    #[case::other_scheme("https://example.com/tmp/a", None)]
    #[case::bad_escape("file:///tmp/%zz", None)]
    #[case::no_path("file://", None)]
    #[cfg(unix)]
    fn test_file_uri_to_path(#[case] uri: &str, #[case] expected: Option<&str>) {
        assert_eq!(file_uri_to_path(uri), expected.map(PathBuf::from));
    }

    #[test]
    fn test_client_roots_enable_access() {
        let (tmp, _) = fixture();
        let access = FileAccess::default();
        assert!(!access.is_enabled());

        let uri = format!("file://{}", tmp.path().join("root").display());
        access.set_client_roots([uri]);
        assert!(access.is_enabled());
        assert_eq!(access.read("a.md").unwrap(), "# A");
        assert!(access.read("../outside/secret.md").is_err());
    }

    #[test]
    fn test_client_roots_skip_invalid_and_can_be_replaced() {
        let (tmp, _) = fixture();
        let access = FileAccess::default();
        let root = format!("file://{}", tmp.path().join("root").display());
        access.set_client_roots([
            root,
            "https://example.com".to_string(),
            format!("file://{}", tmp.path().join("missing").display()),
        ]);
        assert_eq!(access.roots().len(), 1);

        access.set_client_roots(Vec::<String>::new());
        assert!(!access.is_enabled());
    }

    #[test]
    fn test_configured_roots_override_client_roots() {
        let (tmp, access) = fixture();
        let outside = format!("file://{}", tmp.path().join("outside").display());
        access.set_client_roots([outside]);
        assert!(access.read("a.md").is_ok());
        assert!(access.read("secret.md").is_err());
    }

    #[test]
    fn test_for_session_does_not_share_client_roots() {
        let (tmp, _) = fixture();
        let base = FileAccess::default();
        let session = base.for_session();
        session.set_client_roots([format!("file://{}", tmp.path().join("root").display())]);
        assert!(session.is_enabled());
        assert!(!base.is_enabled());
        assert!(!base.for_session().is_enabled());
    }

    #[test]
    fn test_new_rejects_missing_root_and_file_root() {
        let tmp = TempDir::new().unwrap();
        assert!(FileAccess::new([tmp.path().join("missing")], 1).is_err());

        let file = tmp.path().join("f.md");
        fs::write(&file, "").unwrap();
        assert!(
            FileAccess::new([file], 1)
                .unwrap_err()
                .contains("not a directory")
        );
    }

    #[test]
    fn test_list_filters_sorts_and_skips_hidden() {
        let (_tmp, access) = fixture();
        let (files, truncated) = access.list(None, &["md".to_string()], 100).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["a.md", "b.md", "c.MD"]);
        assert!(!truncated);
    }

    #[test]
    fn test_list_truncates_at_limit() {
        let (_tmp, access) = fixture();
        let (files, truncated) = access.list(None, &["md".to_string()], 2).unwrap();
        assert_eq!(files.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn test_list_subdirectory_and_rejects_outside() {
        let (_tmp, access) = fixture();
        let (files, _) = access.list(Some("docs"), &["md".to_string()], 100).unwrap();
        assert_eq!(files.len(), 2);

        assert!(
            access
                .list(Some("../outside"), &["md".to_string()], 100)
                .is_err()
        );
        assert!(access.list(Some("a.md"), &["md".to_string()], 100).is_err());
    }
}
