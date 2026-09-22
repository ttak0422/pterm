use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(format!(
            "/tmp/pterm-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pterm"));
        command.env("PTERM_SOCKET_DIR", self.0.join("sessions"));
        command
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn session_paths_reject_traversal_and_symlinks_before_deleting() {
    let dir = TestDir::new();
    let root = dir.0.join("sessions");
    let outside = dir.0.join("outside");
    std::fs::create_dir_all(root.join("parent/child")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), "untouched").unwrap();
    symlink(&outside, root.join("link")).unwrap();

    for name in [
        "",
        ".",
        "..",
        "../outside",
        "parent/../../outside",
        "parent//child",
        "parent/./child",
        "parent/../child",
        "parent/socket",
        "cwd",
        "link",
        "link/child",
        outside.to_str().unwrap(),
    ] {
        let output = dir.command().args(["kill", name]).output().unwrap();
        assert!(!output.status.success(), "accepted {name:?}");
        assert!(
            outside.join("keep").exists(),
            "deleted outside for {name:?}"
        );
        assert!(
            root.join("parent/child").exists(),
            "deleted root for {name:?}"
        );
    }
    let output = dir
        .command()
        .args(["socket", "parent/child"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        root.join("parent/child/socket").to_str().unwrap()
    );
    assert!(dir
        .command()
        .args(["kill", "parent"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!root.join("parent").exists());
    assert!(outside.join("keep").exists());
}

#[test]
fn list_includes_the_prefix_session_and_skips_directory_symlinks() {
    let dir = TestDir::new();
    let root = dir.0.join("sessions");
    std::fs::create_dir_all(root.join("parent/child")).unwrap();
    let _parent = UnixListener::bind(root.join("parent/socket")).unwrap();
    let _child = UnixListener::bind(root.join("parent/child/socket")).unwrap();
    symlink(&root, root.join("loop")).unwrap();
    for args in [vec!["list"], vec!["list", "parent"]] {
        let output = dir.command().args(args).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"parent\nparent/child\n");
    }
}

#[test]
fn bridge_delivers_buffered_output_and_exit_before_socket_eof() {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    let dir = TestDir::new();
    let session = dir.0.join("sessions/test");
    std::fs::create_dir_all(&session).unwrap();
    let listener = UnixListener::bind(session.join("socket")).unwrap();
    let mut child = dir
        .command()
        .args(["attach", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    // Wait for HELLO + RESIZE, then queue frames and EOF while the bridge is
    // stopped so they deterministically arrive in the same drain cycle.
    let mut handshake = [0; 22];
    stream.read_exact(&mut handshake).unwrap();
    let pid = Pid::from_raw(child.id() as i32);
    kill(pid, Signal::SIGSTOP).unwrap();
    assert_eq!(
        waitpid(pid, Some(WaitPidFlag::WUNTRACED)).unwrap(),
        WaitStatus::Stopped(pid, Signal::SIGSTOP)
    );
    let mut frames = pterm_proto::encode(pterm_proto::server::OUTPUT, b"last output\r\n");
    frames.extend(pterm_proto::encode(
        pterm_proto::server::EXIT,
        &pterm_proto::encode_exit(42),
    ));
    stream.write_all(&frames).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    kill(pid, Signal::SIGCONT).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(42), "{:?}", output);
    assert!(output.stdout.starts_with(b"last output\r\n"));
}

#[test]
fn bridge_preserves_input_frames_until_daemon_resumes_reading() {
    use nix::libc;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc::{self, RecvTimeoutError};

    let dir = TestDir::new();
    let session = dir.0.join("sessions/slow");
    std::fs::create_dir_all(&session).unwrap();
    let listener = UnixListener::bind(session.join("socket")).unwrap();
    let mut child = dir
        .command()
        .args(["attach", "slow"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    let receive_buffer: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&receive_buffer as *const libc::c_int).cast(),
                std::mem::size_of_val(&receive_buffer) as libc::socklen_t,
            )
        },
        0
    );
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut handshake = [0; 22];
    stream.read_exact(&mut handshake).unwrap();

    let expected: Vec<u8> = (0..2 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    let input = expected.clone();
    let (sent, result) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let status = stdin.write_all(&input);
        drop(stdin);
        sent.send(status).unwrap();
    });
    // The daemon is not reading: the bridge must pause stdin, stay connected,
    // and retain the unwritten tail of any partially sent INPUT frame.
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert!(child.try_wait().unwrap().is_none());

    let mut wire = Vec::new();
    stream.read_to_end(&mut wire).unwrap();
    let frames = pterm_proto::decode_frames(&mut wire);
    assert!(wire.is_empty(), "truncated frame after stdin EOF");
    assert_eq!(frames.last().unwrap().msg_type, pterm_proto::client::DETACH);
    let received: Vec<u8> = frames
        .into_iter()
        .filter(|frame| frame.msg_type == pterm_proto::client::INPUT)
        .flat_map(|frame| frame.payload)
        .collect();
    assert_eq!(received, expected);
    result
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
}
