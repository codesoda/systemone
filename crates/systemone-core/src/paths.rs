//! Path resolution shared by backend settings.

use std::path::{Path, PathBuf};

use crate::HostError;

/// Resolve an operator-supplied settings path to an absolute path.
///
/// Absolute paths pass through. A leading `~` expands against `home`; when
/// no home directory is available that is a validation error, never a
/// guess. Any other relative path resolves against the current directory.
///
/// Every local backend must use this resolver so relative paths, `~`
/// expansion, and missing-home behavior cannot drift between backends.
pub fn absolutize(path: &Path, home: Option<&Path>) -> Result<PathBuf, HostError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if let Ok(rest) = path.strip_prefix("~") {
        let home = home.ok_or_else(|| {
            HostError::validation(format!(
                "settings path {} uses `~` but no home directory is available",
                path.display()
            ))
        })?;
        return Ok(home.join(rest));
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| {
            HostError::validation(format!("cannot resolve {}: {error}", path.display()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_pass_through() {
        let path = Path::new(if cfg!(windows) {
            r"C:\models\bundle"
        } else {
            "/models/bundle"
        });
        assert_eq!(absolutize(path, None).unwrap(), PathBuf::from(path));
    }

    #[test]
    fn tilde_expands_against_home() {
        let resolved = absolutize(Path::new("~/models"), Some(Path::new("/home/test"))).unwrap();
        assert_eq!(resolved, PathBuf::from("/home/test/models"));
    }

    #[test]
    fn tilde_without_home_is_a_validation_error() {
        let error = absolutize(Path::new("~/models"), None).unwrap_err();
        assert!(matches!(error, HostError::Validation(_)), "{error}");
    }

    #[test]
    fn relative_paths_resolve_against_the_current_directory() {
        let resolved = absolutize(Path::new("models"), None).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("models"));
    }
}
