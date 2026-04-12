use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// Socket file name within a session directory.
pub const SOCKET_FILENAME: &str = "socket";

pub fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PTERM_SOCKET_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("pterm");
    }
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/pterm-{}", uid))
}

/// Resolve the socket path for a session name.
/// Session name may contain `/` for hierarchical sessions (e.g. "parent/child").
/// Returns: `<socket_dir>/<session_name>/socket`
pub fn session_socket_path(session_name: &str) -> PathBuf {
    socket_dir().join(session_name).join(SOCKET_FILENAME)
}

/// Resolve the session directory for a session name.
pub fn session_dir(session_name: &str) -> PathBuf {
    socket_dir().join(session_name)
}

/// Recursively find all sessions under a directory.
/// Returns session names relative to the socket root directory.
pub fn find_sessions(base: &Path, prefix: &str) -> io::Result<Vec<String>> {
    let mut sessions = Vec::new();
    if !base.exists() {
        return Ok(sessions);
    }

    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        let path = entry.path();
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };

        if name == SOCKET_FILENAME {
            continue;
        }

        if path.is_dir() {
            let full_name = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            let sock = path.join(SOCKET_FILENAME);
            if sock.exists() {
                let meta = std::fs::metadata(&sock)?;
                if meta.file_type().is_socket() {
                    sessions.push(full_name.clone());
                }
            }

            let children = find_sessions(&path, &full_name)?;
            sessions.extend(children);
        }
    }

    Ok(sessions)
}
