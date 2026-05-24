use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// Socket file name within a session directory.
pub const SOCKET_FILENAME: &str = "socket";

/// File name within a session directory recording the session shell's current
/// working directory. Written at creation and refreshed by the daemon.
pub const CWD_FILENAME: &str = "cwd";

/// Look up the current working directory of a running process by pid.
/// Returns `None` if the process is gone or the lookup is unsupported.
#[cfg(target_os = "linux")]
pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    use nix::libc;
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is a zeroed, correctly-sized buffer for PROC_PIDVNODEPATHINFO.
    // proc_pidinfo writes at most `size` bytes and returns the number written.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n < size {
        return None;
    }
    // vip_path is a fixed-size, NUL-terminated C string buffer. Its element type
    // varies across libc versions, so read it through a byte pointer.
    let ptr = (&info.pvi_cdir.vip_path) as *const _ as *const libc::c_char;
    // SAFETY: vip_path is NUL-terminated within the fixed-size buffer above.
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_cwd(_pid: i32) -> Option<PathBuf> {
    None
}

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
