// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Restricted Unix domain socket creation.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Mutex;

static UMASK_LOCK: Mutex<()> = Mutex::new(());

/// Filesystem permissions for a Unix domain socket and its parent directory.
#[derive(Debug, Clone, Copy)]
pub struct SocketPolicy {
    pub mode: u32,
    pub dir_mode: u32,
}

impl Default for SocketPolicy {
    fn default() -> Self {
        Self {
            mode: 0o600,
            dir_mode: 0o700,
        }
    }
}

/// Bind a Unix domain socket and verify its ownership and permissions.
///
/// If the parent directory does not exist, it is created with `dir_mode`.
/// Any failure after the socket is bound removes the socket path before the
/// error is returned.
pub fn bind_restricted(
    path: &Path,
    policy: SocketPolicy,
) -> io::Result<UnixListener> {
    let _guard = UMASK_LOCK
        .lock()
        .map_err(|_| io::Error::other("socket umask lock poisoned"))?;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(policy.dir_mode);
        builder.create(parent)?;
    }

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let old_umask =
        unsafe { libc::umask((!policy.mode & 0o777) as libc::mode_t) };
    let result = UnixListener::bind(path);
    unsafe {
        libc::umask(old_umask);
    }
    let listener = result?;

    if let Err(error) =
        fs::set_permissions(path, Permissions::from_mode(policy.mode))
    {
        return Err(remove_after_error(path, error));
    }

    verify_bound_path(path, policy.mode, unsafe { libc::geteuid() })?;
    Ok(listener)
}

fn verify_bound_path(
    path: &Path,
    expected_mode: u32,
    expected_uid: u32,
) -> io::Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return Err(remove_after_error(path, error)),
    };
    let actual_mode = metadata.permissions().mode() & 0o777;
    let actual_uid = metadata.uid();

    if actual_mode != expected_mode || actual_uid != expected_uid {
        let error = io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "bound socket verification failed: mode {actual_mode:#05o} uid {actual_uid}, expected mode {expected_mode:#05o} uid {expected_uid}",
            ),
        );
        return Err(remove_after_error(path, error));
    }

    Ok(())
}

fn remove_after_error(path: &Path, error: io::Error) -> io::Error {
    match fs::remove_file(path) {
        Ok(()) => error,
        Err(cleanup_error)
            if cleanup_error.kind() == io::ErrorKind::NotFound =>
        {
            error
        }
        Err(cleanup_error) => io::Error::new(
            error.kind(),
            format!("{error}; failed to remove socket: {cleanup_error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn socket_is_mode_0600_after_bind() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");

        let _listener = bind_restricted(&path, SocketPolicy::default())
            .expect("bind restricted socket");
        let mode = fs::metadata(&path)
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[test]
    fn parent_dir_created_with_dir_mode() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let parent = temp.path().join("private");
        let path = parent.join("control.sock");

        let _listener = bind_restricted(&path, SocketPolicy::default())
            .expect("bind restricted socket");
        let mode = fs::metadata(&parent)
            .expect("stat parent directory")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o700);
    }

    #[test]
    fn bind_removes_stale_socket() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");
        let stale = UnixListener::bind(&path).expect("bind stale socket");
        drop(stale);

        let listener = bind_restricted(&path, SocketPolicy::default())
            .expect("replace stale socket");
        let client =
            UnixStream::connect(&path).expect("connect to replacement socket");
        let _accepted =
            listener.accept().expect("accept replacement connection");
        drop(client);
    }

    #[test]
    fn bind_fails_when_mode_verification_fails() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");
        let _listener = UnixListener::bind(&path).expect("bind test socket");
        fs::set_permissions(&path, Permissions::from_mode(0o666))
            .expect("set deliberately unsafe mode");

        let result =
            verify_bound_path(&path, 0o600, unsafe { libc::geteuid() });

        assert!(result.is_err());
        assert!(!path.exists());
    }
}
