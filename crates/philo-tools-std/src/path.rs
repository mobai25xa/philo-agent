//! Root-containment resolution shared by every path-taking tool.
//!
//! Two-phase check (M4 precedent, extended to all tools): a purely lexical
//! containment test first (never touches the filesystem, so escaping paths
//! do not leak existence information), then canonical containment to catch
//! symlinks pointing outside the root.

use std::path::{Component, Path, PathBuf};

pub(crate) enum PathError {
    OutsideRoot,
    NotFound,
    Io(std::io::ErrorKind),
}

/// Resolves `requested` against `root` with the double-containment check.
///
/// With `must_exist`, a missing target reports `NotFound` and the returned
/// path is canonical. Without it (write paths), the deepest existing
/// ancestor is canonicalized for the containment check and the joined path
/// is returned for creation.
pub(crate) fn resolve_in_root(
    root: &Path,
    requested: &str,
    must_exist: bool,
) -> Result<PathBuf, PathError> {
    let requested_path = Path::new(requested);
    let joined = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };

    let absolute_root = std::path::absolute(root).map_err(io)?;
    let absolute_target = std::path::absolute(&joined).map_err(io)?;
    match (
        normalize_lexically(&absolute_root),
        normalize_lexically(&absolute_target),
    ) {
        (Some(root), Some(target)) if target.starts_with(&root) => {}
        _ => return Err(PathError::OutsideRoot),
    }

    let canonical_root = root.canonicalize().map_err(io)?;
    match joined.canonicalize() {
        Ok(canonical) => {
            if canonical.starts_with(&canonical_root) {
                Ok(canonical)
            } else {
                Err(PathError::OutsideRoot)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if must_exist {
                return Err(PathError::NotFound);
            }
            // Creation path: canonicalize the deepest existing ancestor and
            // re-attach the non-existing suffix for the containment check.
            let mut existing = joined.parent();
            let mut suffix = vec![joined.file_name().ok_or(PathError::OutsideRoot)?];
            let canonical_ancestor = loop {
                let Some(ancestor) = existing else {
                    return Err(PathError::OutsideRoot);
                };
                match ancestor.canonicalize() {
                    Ok(canonical) => break canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        suffix.push(ancestor.file_name().ok_or(PathError::OutsideRoot)?);
                        existing = ancestor.parent();
                    }
                    Err(error) => return Err(io(error)),
                }
            };
            if !canonical_ancestor.starts_with(&canonical_root) {
                return Err(PathError::OutsideRoot);
            }
            let mut resolved = canonical_ancestor;
            for part in suffix.into_iter().rev() {
                resolved.push(part);
            }
            Ok(resolved)
        }
        Err(error) => Err(io(error)),
    }
}

fn io(error: std::io::Error) -> PathError {
    PathError::Io(error.kind())
}

/// Removes `.` segments and resolves `..` lexically. Returns `None` when a
/// `..` would climb past the path root.
pub(crate) fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                normalized.pop();
                depth -= 1;
            }
            Component::Normal(part) => {
                normalized.push(part);
                depth += 1;
            }
        }
    }
    Some(normalized)
}
