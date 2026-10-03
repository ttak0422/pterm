mod bridge;
mod constants;
mod paths;
mod pty;
mod server;
mod session;

use crate::paths::{
    find_sessions, session_dir, session_socket_path, socket_dir, CWD_FILENAME, SOCKET_FILENAME,
};
use server::Server;
use session::Session;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::time::{Duration, Instant};

fn print_usage() {
    eprintln!(
        "pterm - persistent terminal daemon

Usage:
  pterm new    <session-name> [--] <command> [args...]
  pterm attach <session-name>
               # attach to session (bridge mode)
  pterm open   <session-name> [--] <command> [args...]
               # attach if exists, otherwise create and attach
  pterm resize <session-name> --cols N --rows N
               # set the authoritative session dimensions
  pterm list   [prefix]
  pterm kill   <session-name>
  pterm redraw <session-name>   # redraw terminal (resend snapshot)
  pterm dump   <session-name>   # print diagnostic state dump as JSON
  pterm snapshot-text <session-name>
               # print plain-text snapshot of current screen
  pterm full-text <session-name>
               # print plain-text scrollback + screen contents
  pterm snapshot-ansi <session-name>
               # print snapshot of current screen with ANSI colors/attributes
  pterm socket <session-name>   # print socket path

Session names may contain '/' for hierarchical sessions:
  pterm new    parent
  pterm new    parent/child
  pterm kill   parent          # kills parent and all children

Environment:
  PTERM_SOCKET_DIR   Override socket directory
  SHELL              Default command if none specified"
    );
}

fn cmd_new(args: &[String], quiet: bool) -> io::Result<()> {
    let mut session_name = String::new();
    let mut cmd_args: Vec<String> = Vec::new();
    let mut parsing_opts = true;

    let mut i = 0;
    while i < args.len() {
        if parsing_opts && args[i] == "--" {
            parsing_opts = false;
            i += 1;
            continue;
        }
        if session_name.is_empty() {
            session_name = args[i].clone();
        } else {
            cmd_args.push(args[i].clone());
        }
        i += 1;
    }

    if session_name.is_empty() {
        eprintln!("Error: session name required");
        std::process::exit(1);
    }

    // Default command
    if cmd_args.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        cmd_args.push(shell);
    }

    let sess_dir = session_dir(&session_name)?;
    let sock_path = sess_dir.join(SOCKET_FILENAME);

    // Clean up stale socket file from pre-hierarchy daemon layout.
    // Old daemons created the socket directly at `<socket_dir>/<name>` instead
    // of `<socket_dir>/<name>/socket`. Remove it so we can create the directory.
    if sess_dir.exists() && !sess_dir.is_dir() {
        let meta = std::fs::symlink_metadata(&sess_dir)?;
        if meta.file_type().is_socket() {
            std::fs::remove_file(&sess_dir)?;
        } else {
            eprintln!(
                "Error: '{}' exists and is not a directory",
                sess_dir.display()
            );
            std::process::exit(1);
        }
    }

    if sock_path.exists() {
        eprintln!("Error: session '{}' already exists", session_name);
        std::process::exit(1);
    }

    // Create session directory (including parent directories for hierarchical names)
    std::fs::create_dir_all(&sess_dir)?;

    // Record the initial working directory so clients can show which directory
    // a session belongs to (e.g. distinguishing git worktrees that share names).
    // The daemon refreshes this as the shell changes directory.
    if let Ok(cwd) = std::env::current_dir() {
        let _ = std::fs::write(
            sess_dir.join(CWD_FILENAME),
            cwd.to_string_lossy().as_bytes(),
        );
    }

    // Daemonize: fork into background
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            // Parent: print info and return.
            // Suppress output when called from cmd_open to avoid JSON
            // leaking into the Neovim terminal buffer.
            if !quiet {
                println!(
                    "{}",
                    serde_json::json!({
                        "session": session_name,
                        "pid": child.as_raw(),
                        "socket": sock_path.to_string_lossy(),
                    })
                );
            }
            return Ok(());
        }
        Ok(nix::unistd::ForkResult::Child) => {
            // Child: become daemon
            nix::unistd::setsid().ok();

            // Close stdin/stdout/stderr
            let devnull = std::fs::File::open("/dev/null").unwrap();
            nix::unistd::dup2_stdin(&devnull).ok();
            nix::unistd::dup2_stdout(&devnull).ok();
            nix::unistd::dup2_stderr(&devnull).ok();
        }
        Err(e) => {
            eprintln!("Fork failed: {}", e);
            std::process::exit(1);
        }
    }

    // Now running as daemon
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let cmd = &cmd_args[0];
    let str_args: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();

    let session = Session::new(session_name, cmd, &str_args)?;
    let mut server = Server::new(&sess_dir, session)?;
    server.run()?;

    Ok(())
}

fn cmd_list(args: &[String]) -> io::Result<()> {
    let sock_dir = socket_dir();
    let prefix = args.first().map(|s| s.as_str()).unwrap_or("");

    let search_dir = if prefix.is_empty() {
        sock_dir
    } else {
        session_dir(prefix)?
    };

    let mut sessions = find_sessions(&search_dir, prefix)?;
    sessions.sort();
    for name in sessions {
        println!("{}", name);
    }
    Ok(())
}

fn cmd_kill(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sess_dir = session_dir(name)?;

    if !sess_dir.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    // Recursively remove the session directory (kills parent + all children)
    // The daemon(s) will detect socket removal and shut down.
    std::fs::remove_dir_all(&sess_dir)?;

    // Try to clean up empty parent directories
    let sock_root = socket_dir();
    let mut parent = sess_dir.parent();
    while let Some(p) = parent {
        if p == sock_root {
            break;
        }
        // Only remove if empty
        if std::fs::read_dir(p)?.next().is_none() {
            std::fs::remove_dir(p).ok();
        } else {
            break;
        }
        parent = p.parent();
    }

    println!("Session '{}' killed", name);
    Ok(())
}

fn wait_for_socket(sock: &Path, timeout: Duration, poll: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if sock.exists() {
            let meta = std::fs::metadata(sock)?;
            if meta.file_type().is_socket() {
                return Ok(true);
            }
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

#[derive(Default)]
struct AttachOptions {
    name: String,
    cols: Option<u16>,
    rows: Option<u16>,
    no_resize: bool,
    command: Vec<String>,
}

fn parse_attach_options(args: &[String]) -> io::Result<AttachOptions> {
    let mut options = AttachOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                if options.name.is_empty() {
                    i += 1;
                    options.name = args.get(i).cloned().unwrap_or_default();
                }
                options
                    .command
                    .extend_from_slice(args.get(i + 1..).unwrap_or_default());
                break;
            }
            "--no-resize" => options.no_resize = true,
            "--cols" | "--rows" => {
                let flag = &args[i];
                i += 1;
                let value = args
                    .get(i)
                    .and_then(|s| s.parse::<u16>().ok())
                    .filter(|&n| n != 0)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{flag} requires a positive integer"),
                        )
                    })?;
                if flag == "--cols" {
                    options.cols = Some(value);
                } else {
                    options.rows = Some(value);
                }
            }
            _ if options.name.is_empty() => options.name = args[i].clone(),
            _ => {
                // Preserve the historical `open session command args` form.
                options.command.extend_from_slice(&args[i..]);
                break;
            }
        }
        i += 1;
    }
    if options.name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name required",
        ));
    }
    if options
        .cols
        .is_some_and(|n| n > constants::MAX_TERMINAL_COLS)
        || options
            .rows
            .is_some_and(|n| n > constants::MAX_TERMINAL_ROWS)
        || options.cols.zip(options.rows).is_some_and(|(cols, rows)| {
            usize::from(cols) * usize::from(rows) > constants::MAX_TERMINAL_CELLS
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal dimensions exceed the supported limits",
        ));
    }
    Ok(options)
}

fn cmd_attach(args: &[String]) -> io::Result<()> {
    let options = parse_attach_options(args)?;
    let sock = session_socket_path(&options.name)?;
    let exit_code = bridge::run(&sock, options.cols, options.rows, !options.no_resize)?;
    std::process::exit(exit_code);
}

fn cmd_open(args: &[String]) -> io::Result<()> {
    let options = parse_attach_options(args)?;
    let sock = session_socket_path(&options.name)?;
    if !sock.exists() {
        let mut new_args = vec![options.name.clone(), "--".to_string()];
        new_args.extend(options.command);
        cmd_new(&new_args, true)?;
        if !wait_for_socket(
            &sock,
            Duration::from_millis(3000),
            Duration::from_millis(50),
        )? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "session '{}' was created but socket did not appear in time",
                    options.name
                ),
            ));
        }
    }
    let exit_code = bridge::run(&sock, options.cols, options.rows, !options.no_resize)?;
    std::process::exit(exit_code);
}

fn cmd_resize(args: &[String]) -> io::Result<()> {
    let options = parse_attach_options(args)?;
    let (cols, rows) = options.cols.zip(options.rows).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "resize requires --cols and --rows",
        )
    })?;
    let socket_path = session_socket_path(&options.name)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match std::os::unix::net::UnixStream::connect(&socket_path) {
            Ok(stream) => break stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    };
    let payload = pterm_proto::encode_resize(cols, rows);
    stream.write_all(&pterm_proto::encode(
        pterm_proto::client::SET_SIZE,
        &payload,
    ))?;
    // Broadcast snapshots may precede our request under active PTY output.
    // Only the dedicated response to this controller connection acknowledges
    // completion, so serialized focus changes cannot overtake one another.
    let ack = read_single_response(
        &mut stream,
        pterm_proto::server::RESIZE_ACK,
        Duration::from_secs(3),
    )?;
    if ack != payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resize acknowledgement does not match request",
        ));
    }
    Ok(())
}

fn cmd_redraw(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name)?;
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&sock)?;
    let msg = pterm_proto::encode(pterm_proto::client::REDRAW, &[]);
    std::io::Write::write_all(&mut stream, &msg)?;
    Ok(())
}

fn read_single_response(
    stream: &mut std::os::unix::net::UnixStream,
    expected_msg_type: u8,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    // Overall deadline, not per-read: a daemon that keeps streaming OUTPUT
    // frames but never answers the request (e.g. a pre-upgrade daemon that
    // ignores an unknown message type) must not extend the wait forever.
    let deadline = Instant::now() + timeout;

    let mut recv_buf = Vec::new();
    let mut read_buf = vec![0u8; 65536];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for daemon response",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;

        match stream.read(&mut read_buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed connection before sending response",
                ));
            }
            Ok(n) => {
                recv_buf.extend_from_slice(&read_buf[..n]);
                for frame in
                    pterm_proto::decode_frames(&mut recv_buf, pterm_proto::MAX_SERVER_PAYLOAD)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                {
                    if frame.msg_type == expected_msg_type {
                        return Ok(frame.payload);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for daemon response",
                ));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Send a payload-less request to a session's daemon and print the single
/// response payload to stdout. Shared by `dump`, `snapshot-text`, and
/// `full-text`.
fn cmd_query(args: &[String], request: u8, response: u8) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name)?;
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&sock)?;
    let msg = pterm_proto::encode(request, &[]);
    stream.write_all(&msg)?;

    let payload = read_single_response(&mut stream, response, Duration::from_secs(3))?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&payload)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn cmd_socket(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock_path = session_socket_path(name)?;
    println!("{}", sock_path.display());
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    let result = match args[1].as_str() {
        "new" => cmd_new(&args[2..], false),
        "attach" => cmd_attach(&args[2..]),
        "open" => cmd_open(&args[2..]),
        "list" | "ls" => cmd_list(&args[2..]),
        "kill" => cmd_kill(&args[2..]),
        "redraw" => cmd_redraw(&args[2..]),
        "resize" => cmd_resize(&args[2..]),
        "dump" => cmd_query(
            &args[2..],
            pterm_proto::client::DUMP,
            pterm_proto::server::DUMP,
        ),
        "snapshot-text" => cmd_query(
            &args[2..],
            pterm_proto::client::SNAPSHOT_TEXT,
            pterm_proto::server::SNAPSHOT_TEXT,
        ),
        "full-text" => cmd_query(
            &args[2..],
            pterm_proto::client::FULL_TEXT,
            pterm_proto::server::FULL_TEXT,
        ),
        "snapshot-ansi" => cmd_query(
            &args[2..],
            pterm_proto::client::SNAPSHOT_ANSI,
            pterm_proto::server::SNAPSHOT_ANSI,
        ),
        "socket" => cmd_socket(&args[2..]),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod resize_options_tests {
    use super::parse_attach_options;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn managed_options_do_not_leak_into_the_child_command() {
        let parsed = parse_attach_options(&args(&[
            "dev",
            "--no-resize",
            "--cols",
            "1",
            "--rows",
            "24",
            "--",
            "sh",
            "-c",
            "echo ok",
        ]))
        .unwrap();
        assert_eq!(parsed.name, "dev");
        assert_eq!(parsed.cols, Some(1));
        assert_eq!(parsed.rows, Some(24));
        assert!(parsed.no_resize);
        assert_eq!(parsed.command, args(&["sh", "-c", "echo ok"]));
    }

    #[test]
    fn legacy_command_arguments_and_separator_remain_supported() {
        for input in [
            args(&["dev", "sh", "-c", "echo ok"]),
            args(&["--", "dev", "sh", "-c", "echo ok"]),
        ] {
            let parsed = parse_attach_options(&input).unwrap();
            assert_eq!(parsed.name, "dev");
            assert_eq!(parsed.command, args(&["sh", "-c", "echo ok"]));
            assert!(!parsed.no_resize);
        }
    }

    #[test]
    fn invalid_dimensions_are_rejected_before_contacting_a_daemon() {
        for input in [
            vec!["dev", "--cols", "0"],
            vec!["dev", "--cols"],
            vec!["dev", "--rows", "abc"],
            vec!["dev", "--cols", "513"],
            vec!["dev", "--rows", "257"],
            vec!["dev", "--cols", "512", "--rows", "256"],
        ] {
            assert!(parse_attach_options(&args(&input)).is_err());
        }
    }
}
