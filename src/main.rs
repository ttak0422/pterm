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
  pterm list   [prefix]
  pterm kill   <session-name>
  pterm redraw <session-name>   # redraw terminal (resend snapshot)
  pterm dump   <session-name>   # print diagnostic state dump as JSON
  pterm snapshot-text <session-name>
               # print plain-text snapshot of current screen
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

    let sess_dir = session_dir(&session_name);
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
            nix::unistd::dup2(devnull.as_raw_fd(), 0).ok();
            nix::unistd::dup2(devnull.as_raw_fd(), 1).ok();
            nix::unistd::dup2(devnull.as_raw_fd(), 2).ok();
        }
        Err(e) => {
            eprintln!("Fork failed: {}", e);
            std::process::exit(1);
        }
    }

    // Now running as daemon
    use std::os::fd::AsRawFd;

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
        sock_dir.join(prefix)
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

    let sess_dir = session_dir(name);

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

/// Extract session name from args following the same parsing rule as `cmd_new`:
/// first non-option argument, ignoring an optional `--` separator.
fn parse_session_name(args: &[String]) -> Option<&str> {
    let mut parsing_opts = true;
    let mut i = 0;
    while i < args.len() {
        if parsing_opts && args[i] == "--" {
            parsing_opts = false;
            i += 1;
            continue;
        }
        return Some(args[i].as_str());
    }
    None
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

fn cmd_attach(args: &[String]) -> io::Result<()> {
    let mut session_name = String::new();

    let mut i = 0;
    while i < args.len() {
        if session_name.is_empty() {
            session_name = args[i].clone();
        }
        i += 1;
    }

    if session_name.is_empty() {
        eprintln!("Error: session name required");
        std::process::exit(1);
    }

    let sock = session_socket_path(&session_name);
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", session_name);
        std::process::exit(1);
    }

    let exit_code = bridge::run(&sock, None, None)?;
    std::process::exit(exit_code);
}

fn cmd_open(args: &[String]) -> io::Result<()> {
    let name = parse_session_name(args).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
    if !sock.exists() {
        cmd_new(args, true)?;
        let ok = wait_for_socket(
            &sock,
            Duration::from_millis(3000),
            Duration::from_millis(50),
        )?;
        if !ok {
            eprintln!(
                "Error: session '{}' was created but socket did not appear in time",
                name
            );
            std::process::exit(1);
        }
    }

    let exit_code = bridge::run(&sock, None, None)?;
    std::process::exit(exit_code);
}

fn cmd_redraw(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
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
    stream.set_read_timeout(Some(timeout))?;

    let mut recv_buf = Vec::new();
    let mut read_buf = vec![0u8; 65536];
    loop {
        match stream.read(&mut read_buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed connection before sending response",
                ));
            }
            Ok(n) => {
                recv_buf.extend_from_slice(&read_buf[..n]);
                for frame in pterm_proto::decode_frames(&mut recv_buf) {
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

fn cmd_dump(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&sock)?;
    let msg = pterm_proto::encode(pterm_proto::client::DUMP, &[]);
    stream.write_all(&msg)?;

    let payload = read_single_response(
        &mut stream,
        pterm_proto::server::DUMP,
        Duration::from_secs(3),
    )?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&payload)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn cmd_snapshot_text(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&sock)?;
    let msg = pterm_proto::encode(pterm_proto::client::SNAPSHOT_TEXT, &[]);
    stream.write_all(&msg)?;

    let payload = read_single_response(
        &mut stream,
        pterm_proto::server::SNAPSHOT_TEXT,
        Duration::from_secs(3),
    )?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&payload)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn cmd_snapshot_ansi(args: &[String]) -> io::Result<()> {
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&sock)?;
    let msg = pterm_proto::encode(pterm_proto::client::SNAPSHOT_ANSI, &[]);
    stream.write_all(&msg)?;

    let payload = read_single_response(
        &mut stream,
        pterm_proto::server::SNAPSHOT_ANSI,
        Duration::from_secs(3),
    )?;
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

    let sock_path = session_socket_path(name);
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
        "dump" => cmd_dump(&args[2..]),
        "snapshot-text" => cmd_snapshot_text(&args[2..]),
        "snapshot-ansi" => cmd_snapshot_ansi(&args[2..]),
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
