use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub struct PathJail {
    allowed_roots: Vec<PathBuf>,
}

impl PathJail {
    pub fn new(roots: Vec<PathBuf>) -> io::Result<Self> {
        let allowed_roots = roots
            .into_iter()
            .map(fs::canonicalize)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { allowed_roots })
    }

    pub fn verify_path(&self, candidate: &Path) -> Result<PathBuf, String> {
        if self.allowed_roots.is_empty() {
            return Err("SECURITY_VIOLATION: no workspace roots are allowed".to_owned());
        }

        let target = resolve_candidate(candidate).map_err(|error| {
            format!(
                "SECURITY_VIOLATION: could not safely resolve path {:?}: {error}",
                candidate
            )
        })?;

        if self
            .allowed_roots
            .iter()
            .any(|root| target.starts_with(root))
        {
            Ok(target)
        } else {
            Err(format!(
                "SECURITY_VIOLATION: path {:?} is outside mounted workspace roots",
                candidate
            ))
        }
    }
}

fn resolve_candidate(candidate: &Path) -> io::Result<PathBuf> {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()?.join(candidate)
    };

    let mut components = absolute.components().peekable();
    let mut resolved = PathBuf::new();

    if let Some(Component::Prefix(prefix)) = components.peek().copied() {
        resolved.push(prefix.as_os_str());
        components.next();
    }
    if let Some(Component::RootDir) = components.peek().copied() {
        resolved.push(Path::new(std::path::MAIN_SEPARATOR_STR));
        components.next();
    }
    resolved = fs::canonicalize(&resolved)?;

    let mut missing_components = Vec::new();
    for component in components {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unexpected root component in absolute path",
                ));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if missing_components.pop().is_none() {
                    resolved = fs::canonicalize(resolved.join(".."))?;
                }
            }
            Component::Normal(name) => {
                #[cfg(windows)]
                if name.to_string_lossy().contains(':') {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "NTFS alternate data streams are not allowed",
                    ));
                }

                if !missing_components.is_empty() {
                    missing_components.push(name.to_os_string());
                    continue;
                }

                let next = resolved.join(name);
                match fs::canonicalize(&next) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        match fs::symlink_metadata(&next) {
                            Ok(_) => return Err(error),
                            Err(metadata_error)
                                if metadata_error.kind() == io::ErrorKind::NotFound =>
                            {
                                missing_components.push(name.to_os_string());
                            }
                            Err(metadata_error) => return Err(metadata_error),
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    for component in missing_components {
        resolved.push(component);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("jcode-path-jail-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn empty_roots_deny_everything() {
        let jail = PathJail::new(Vec::new()).expect("empty roots are valid");
        assert!(jail.verify_path(Path::new("some-file")).is_err());
    }

    #[test]
    fn permits_existing_and_new_paths_inside_root() {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("create workspace");
        let existing = root.join("existing.txt");
        fs::write(&existing, "content").expect("create existing file");

        let jail = PathJail::new(vec![root.clone()]).expect("canonicalize workspace");
        assert_eq!(
            jail.verify_path(&existing).unwrap(),
            existing.canonicalize().unwrap()
        );

        let new_path = root.join("new").join("file.txt");
        assert_eq!(jail.verify_path(&new_path).unwrap(), new_path);
    }

    #[test]
    fn rejects_parent_traversal_and_component_prefix_siblings() {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        let sibling = temp.path().join("workspace-private");
        fs::create_dir(&root).expect("create workspace");
        fs::create_dir(&sibling).expect("create sibling");

        let jail = PathJail::new(vec![root.clone()]).expect("canonicalize workspace");
        for candidate in [
            sibling.join("secret.txt"),
            root.join("..").join("workspace-private").join("new.txt"),
            root.join("not-created")
                .join("..")
                .join("..")
                .join("workspace-private")
                .join("new.txt"),
        ] {
            assert!(
                jail.verify_path(&candidate).is_err(),
                "accepted {candidate:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).expect("create workspace");
        fs::create_dir(&outside).expect("create outside directory");
        symlink(&outside, root.join("escape")).expect("create symlink");

        let jail = PathJail::new(vec![root.clone()]).expect("canonicalize workspace");
        assert!(
            jail.verify_path(&root.join("escape").join("secret.txt"))
                .is_err()
        );
    }

    proptest::proptest! {
        #[test]
        fn successful_resolution_never_escapes_canonical_root(suffix in "[A-Za-z0-9._:/\\\\-]{0,80}") {
            let temp = TestDir::new();
            let root = temp.path().join("workspace");
            fs::create_dir(&root).expect("create workspace");
            let canonical_root = root.canonicalize().expect("canonicalize workspace");
            let jail = PathJail::new(vec![root.clone()]).expect("create path jail");
            let candidate = root.join(suffix);

            if let Ok(resolved) = jail.verify_path(&candidate) {
                proptest::prop_assert!(resolved.starts_with(&canonical_root));
            }
        }
    }
    /// Property 1 (adversarial, 1,000 cases): for any input containing `..`,
    /// mixed separators, or deep nesting, `verify_path` must never return
    /// `Ok(path)` whose canonical prefix is outside an allowed root.
    #[test]
    fn property1_traversal_invariant_one_thousand_cases() {
        fn check(jail: &PathJail, canonical_root: &Path, input: &str) {
            if let Ok(resolved) = jail.verify_path(Path::new(input)) {
                assert!(
                    resolved.starts_with(canonical_root),
                    "input {:?} escaped to {:?}",
                    input,
                    resolved
                );
            }
        }

        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("create workspace");
        let canonical_root = root.canonicalize().expect("canonicalize workspace");
        let jail = PathJail::new(vec![root.clone()]).expect("create path jail");

        let sibling = temp.path().join("workspace-private");
        fs::create_dir(&sibling).expect("create sibling");
        fs::write(sibling.join("secret.txt"), "top-secret").expect("write secret");

        let mut cases: Vec<String> = Vec::new();
        for i in 0..1000u32 {
            let depth = (i % 6) + 1;
            let mut s = String::new();
            for _ in 0..depth {
                s.push_str("..");
                s.push_str(if i % 2 == 0 { "/" } else { "\\" });
            }
            s.push_str("workspace-private");
            s.push_str(if i % 3 == 0 { "/" } else { "\\" });
            s.push_str("secret.txt");
            cases.push(s.clone());
            cases.push(format!(
                "workspace{}{}",
                if i % 2 == 0 { "/" } else { "\\" },
                s
            ));
        }
        cases.sort();
        cases.dedup();
        let cases: Vec<String> = cases.into_iter().take(1000).collect();

        for input in &cases {
            check(&jail, &canonical_root, input);
        }
    }

    /// Explicit named red-team vectors (Property 1).
    #[test]
    fn redteam_named_traversal_vectors() {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("create workspace");
        let canonical_root = root.canonicalize().expect("canonicalize workspace");
        let jail = PathJail::new(vec![root.clone()]).expect("create path jail");

        let sibling = temp.path().join("workspace-private");
        fs::create_dir(&sibling).expect("create sibling");
        fs::write(sibling.join("secret.txt"), "top-secret").expect("write secret");

        let vectors: Vec<PathBuf> = vec![
            root.join("..")
                .join("..")
                .join("workspace-private")
                .join("secret.txt"),
            root.join("..").join("workspace-private").join("secret.txt"),
            Path::new("workspace/../../workspace-private/secret.txt").to_path_buf(),
            Path::new("workspace\\..\\..\\workspace-private\\secret.txt").to_path_buf(),
            root.join("a")
                .join("b")
                .join("..")
                .join("..")
                .join("..")
                .join("workspace-private")
                .join("secret.txt"),
            sibling.join("secret.txt"),
        ];

        for candidate in vectors {
            if let Ok(resolved) = jail.verify_path(&candidate) {
                assert!(
                    resolved.starts_with(&canonical_root),
                    "traversal vector {:?} escaped to {:?}",
                    candidate,
                    resolved
                );
            }
        }
    }

    /// NTFS Alternate Data Stream rejection (Windows-specific code path).
    #[cfg(windows)]
    #[test]
    fn redteam_ads_stream_is_rejected() {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("create workspace");
        let jail = PathJail::new(vec![root.clone()]).expect("create path jail");
        fs::write(root.join("allowed_file.txt"), "benign").expect("write file");
        let ads = Path::new(r"allowed_file.txt:hidden_stream:$DATA");
        assert!(
            jail.verify_path(ads).is_err(),
            "ADS stream must be rejected"
        );
    }

    /// Extended-length UNC prefix manipulation rejection (Windows-specific).
    #[cfg(windows)]
    #[test]
    fn redteam_extended_length_unc_is_rejected() {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("create workspace");
        let jail = PathJail::new(vec![root.clone()]).expect("create path jail");
        let unc = Path::new(r"\\?\C:\Windows\win.ini");
        assert!(
            jail.verify_path(unc).is_err(),
            "extended-length UNC system path must be rejected"
        );
    }

    /// Directory junction escape rejection (Windows-specific).
    #[cfg(windows)]
    #[test]
    fn redteam_directory_junction_escape_is_rejected() {
        use std::os::windows::fs::symlink_dir;
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).expect("create workspace");
        fs::create_dir(&outside).expect("create outside directory");
        fs::write(outside.join("secret.txt"), "junction-secret").expect("write secret");
        symlink_dir(&outside, root.join("escape")).expect("create junction");
        let jail = PathJail::new(vec![root.clone()]).expect("create path jail");
        assert!(
            jail.verify_path(&root.join("escape").join("secret.txt"))
                .is_err(),
            "directory junction escape must be rejected"
        );
    }
}
