use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::UnixListener;

/// Binds a local control socket after safely removing a stale socket node.
///
/// # Errors
///
/// Returns an error for filesystem failures, a live listener, or when `path`
/// exists but is not a Unix socket.
pub fn bind_control_socket(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(parent)?;
    }
    remove_control_socket(path)?;
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Removes `path` only when it is a Unix socket.
///
/// # Errors
///
/// Returns an error for filesystem failures, a live listener, or when `path`
/// exists but is not a Unix socket.
pub fn remove_control_socket(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another host daemon is listening at {}", path.display()),
                )),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path)
                }
                Err(error) => Err(error),
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to replace non-socket path {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn refuses_to_delete_a_regular_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("control.sock");
        fs::write(&path, "important").unwrap();

        let error = bind_control_socket(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(path).unwrap(), "important");
    }

    #[tokio::test]
    async fn replaces_a_stale_socket() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("control.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(bind_control_socket(&path).is_ok());
    }

    #[test]
    fn refuses_to_unlink_an_active_socket() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let error = bind_control_socket(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());
        drop(listener);
    }
}
