use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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
fn bridge_and_query_reject_oversized_response_headers_without_payload() {
    for command in ["attach", "dump"] {
        let dir = TestDir::new();
        let session = dir.0.join("sessions/oversized");
        std::fs::create_dir_all(&session).unwrap();
        let listener = UnixListener::bind(session.join("socket")).unwrap();
        let mut child = dir
            .command()
            .args([command, "oversized"])
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
        let mut request = vec![0; if command == "attach" { 22 } else { 5 }];
        stream.read_exact(&mut request).unwrap();
        let mut header = vec![pterm_proto::server::OUTPUT];
        header.extend_from_slice(&((pterm_proto::MAX_SERVER_PAYLOAD + 1) as u32).to_le_bytes());
        stream.write_all(&header).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut timed_out = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                timed_out = true;
                child.kill().unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!timed_out, "{command} waited for an oversized payload");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("exceeds limit"),
            "{command}: {:?}",
            output
        );
    }
}

#[test]
fn bridge_preserves_fragmented_history_before_snapshot_and_exit() {
    let dir = TestDir::new();
    let session = dir.0.join("sessions/history");
    std::fs::create_dir_all(&session).unwrap();
    let listener = UnixListener::bind(session.join("socket")).unwrap();
    let mut child = dir
        .command()
        .args(["attach", "history"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    let history = vec![b'h'; 128 * 1024];
    let expected = history.clone();
    let daemon = std::thread::spawn(move || {
        let mut request = [0; 22];
        stream.read_exact(&mut request).unwrap();
        let mut wire = pterm_proto::encode(
            pterm_proto::server::HELLO_ACK,
            &pterm_proto::encode_hello_ack(pterm_proto::PROTO_VERSION, "test"),
        );
        wire.extend(pterm_proto::encode(pterm_proto::server::HISTORY, &history));
        wire.extend(pterm_proto::encode(
            pterm_proto::server::STATE_SYNC,
            b"screen",
        ));
        wire.extend(pterm_proto::encode(
            pterm_proto::server::EXIT,
            &pterm_proto::encode_exit(17),
        ));
        for chunk in wire.chunks(1031) {
            stream.write_all(chunk).unwrap();
        }
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let output = child.wait_with_output().unwrap();
    daemon.join().unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert!(output.stdout.starts_with(&expected));
    assert!(output.stdout[expected.len()..]
        .windows(6)
        .any(|bytes| bytes == b"screen"));
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
    let frames = pterm_proto::decode_frames(&mut wire, pterm_proto::MAX_CLIENT_PAYLOAD).unwrap();
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

#[test]
fn bridge_keeps_input_and_resize_responsive_with_stopped_stdout() {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::libc;
    use nix::sys::signal::{kill, Signal};
    use nix::sys::termios;
    use nix::unistd::Pid;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Instant;

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    for terminal in [false, true] {
        let dir = TestDir::new();
        let session = dir.0.join("sessions/output");
        std::fs::create_dir_all(&session).unwrap();
        let listener = UnixListener::bind(session.join("socket")).unwrap();
        let (read, write) = if terminal {
            let pty = nix::pty::openpty(None, None).unwrap();
            (pty.master, pty.slave)
        } else {
            nix::unistd::pipe().unwrap()
        };
        let original_flags = fcntl(&write, FcntlArg::F_GETFL).unwrap();
        let original_termios = terminal.then(|| termios::tcgetattr(&write).unwrap());
        let mut command = dir.command();
        command
            .args(["attach", "output"])
            .stdout(Stdio::from(write.try_clone().unwrap()));
        if terminal {
            command.stdin(Stdio::from(write.try_clone().unwrap()));
        } else {
            command.stdin(Stdio::piped());
        }
        let mut child = ChildGuard(command.stderr(Stdio::null()).spawn().unwrap());
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut handshake = [0; 22];
        stream.read_exact(&mut handshake).unwrap();

        let keyboard_cleanup = b"\x1b[<u\x1b[=0u";
        let mut expected = b"history".to_vec();
        expected.extend_from_slice(keyboard_cleanup);
        expected.extend_from_slice(b"snapshot");
        let payload = vec![b'x'; 16 * 1024];
        let mut frames = pterm_proto::encode(
            pterm_proto::server::HELLO_ACK,
            &pterm_proto::encode_hello_ack(pterm_proto::PROTO_VERSION, "test"),
        );
        frames.extend(pterm_proto::encode(
            pterm_proto::server::HISTORY,
            b"history",
        ));
        frames.extend(pterm_proto::encode(
            pterm_proto::server::STATE_SYNC,
            b"snapshot",
        ));
        for _ in 0..256 {
            frames.extend(pterm_proto::encode(pterm_proto::server::OUTPUT, &payload));
            expected.extend_from_slice(&payload);
        }
        frames.extend(pterm_proto::encode(
            pterm_proto::server::EXIT,
            &pterm_proto::encode_exit(42),
        ));
        let mut producer = stream.try_clone().unwrap();
        let (sent, result) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let status = producer.write_all(&frames);
            let _ = sent.send(status);
        });
        // The producer cannot drain 4 MiB into an unbounded bridge queue.
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(150)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(child.0.try_wait().unwrap().is_none());
        if terminal {
            let size = libc::winsize {
                ws_row: 30,
                ws_col: 90,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(write.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                0
            );
            kill(Pid::from_raw(child.0.id() as i32), Signal::SIGWINCH).unwrap();
            nix::unistd::write(&read, b"probe").unwrap();
        } else {
            child.0.stdin.as_mut().unwrap().write_all(b"probe").unwrap();
        }
        let mut incoming = Vec::new();
        let mut got_input = false;
        let mut got_resize = !terminal;
        while !got_input || !got_resize {
            let mut buf = [0; 1024];
            let n = stream
                .read(&mut buf)
                .expect("bridge blocked on stdout instead of handling input/signal");
            assert!(n > 0);
            incoming.extend_from_slice(&buf[..n]);
            for frame in
                pterm_proto::decode_frames(&mut incoming, pterm_proto::MAX_CLIENT_PAYLOAD).unwrap()
            {
                got_input |=
                    frame.msg_type == pterm_proto::client::INPUT && frame.payload == b"probe";
                got_resize |= frame.msg_type == pterm_proto::client::RESIZE
                    && pterm_proto::parse_resize(&frame.payload).unwrap() == (90, 30);
            }
        }

        // Resume the reader. Read to a deadline because we retain the write fd
        // to verify its shared flags and terminal settings after bridge exit.
        fcntl(
            &read,
            FcntlArg::F_SETFL(
                OFlag::from_bits_retain(fcntl(&read, FcntlArg::F_GETFL).unwrap())
                    | OFlag::O_NONBLOCK,
            ),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut actual = Vec::new();
        let mut status = None;
        loop {
            let mut buf = [0; 65536];
            match nix::unistd::read(&read, &mut buf) {
                Ok(n) if n > 0 => actual.extend_from_slice(&buf[..n]),
                Ok(_) | Err(nix::errno::Errno::EAGAIN) => {
                    if status.is_some() {
                        break;
                    }
                    let mut fd = libc::pollfd {
                        fd: read.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    assert!(unsafe { libc::poll(&mut fd, 1, 20) } >= 0);
                }
                Err(error) => panic!("stdout read failed: {error}"),
            }
            status = child.0.try_wait().unwrap();
            assert!(
                Instant::now() < deadline,
                "bridge did not drain stdout after resume (terminal={terminal}, bytes={}/{}, status={status:?})",
                actual.len(), expected.len()
            );
        }
        result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        writer.join().unwrap();
        assert_eq!(status.unwrap().code(), Some(42));
        assert!(actual.starts_with(&expected));
        let cleanup = &actual[expected.len()..];
        assert!(cleanup.starts_with(b"\x1b[?1000l"));
        assert!(cleanup.ends_with(b"\x1b[<u\x1b[=0u"));
        // Rust's macOS startup may add FNOSIGPIPE; check the flag we change.
        assert_eq!(
            fcntl(&write, FcntlArg::F_GETFL).unwrap() & libc::O_NONBLOCK,
            original_flags & libc::O_NONBLOCK
        );
        if let Some(original) = original_termios {
            let restored = termios::tcgetattr(&write).unwrap();
            assert_eq!(
                restored.local_flags & !termios::LocalFlags::PENDIN,
                original.local_flags & !termios::LocalFlags::PENDIN
            );
            assert_eq!(restored.input_flags, original.input_flags);
            assert_eq!(restored.output_flags, original.output_flags);
        }
    }
}

#[test]
fn bridge_writes_regular_file_stdout_and_restores_flags() {
    use nix::fcntl::{fcntl, FcntlArg};
    let dir = TestDir::new();
    let session = dir.0.join("sessions/file");
    std::fs::create_dir_all(&session).unwrap();
    let listener = UnixListener::bind(session.join("socket")).unwrap();
    let output = std::fs::File::create(dir.0.join("output")).unwrap();
    let original_flags = fcntl(&output, FcntlArg::F_GETFL).unwrap();
    let mut child = dir
        .command()
        .args(["attach", "file"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut handshake = [0; 22];
    stream.read_exact(&mut handshake).unwrap();
    let mut frames = pterm_proto::encode(pterm_proto::server::OUTPUT, b"file output");
    frames.extend(pterm_proto::encode(
        pterm_proto::server::EXIT,
        &pterm_proto::encode_exit(7),
    ));
    stream.write_all(&frames).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("bridge failed to write regular file stdout");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(status.code(), Some(7));
    assert!(std::fs::read(dir.0.join("output"))
        .unwrap()
        .starts_with(b"file output"));
    assert_eq!(
        fcntl(&output, FcntlArg::F_GETFL).unwrap() & nix::libc::O_NONBLOCK,
        original_flags & nix::libc::O_NONBLOCK
    );
}
