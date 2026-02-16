mod bridge;
mod buffer;
mod pty;
mod server;
mod session;

use server::Server;
use session::Session;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PTERM_SOCKET_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("pterm");
    }
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/pterm-{}", uid))
}

/// Socket file name within a session directory.
const SOCKET_FILENAME: &str = "socket";

/// Resolve the socket path for a session name.
/// Session name may contain `/` for hierarchical sessions (e.g. "parent/child").
/// Returns: `<socket_dir>/<session_name>/socket`
fn session_socket_path(session_name: &str) -> PathBuf {
    socket_dir().join(session_name).join(SOCKET_FILENAME)
}

/// Resolve the session directory for a session name.
fn session_dir(session_name: &str) -> PathBuf {
    socket_dir().join(session_name)
}

fn print_usage() {
    eprintln!(
        "pterm - persistent terminal daemon

Usage:
  pterm new    <session-name> [--cols N] [--rows N] [--] <command> [args...]
  pterm attach <session-name>   # attach to session (bridge mode)
  pterm open   <session-name> [--cols N] [--rows N] [--] <command> [args...]
               # attach if exists, otherwise create and attach
  pterm list   [prefix]
  pterm kill   <session-name>
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

fn cmd_new(args: &[String]) -> io::Result<()> {
    let mut session_name = String::new();
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut cmd_args: Vec<String> = Vec::new();
    let mut parsing_opts = true;

    let mut i = 0;
    while i < args.len() {
        if parsing_opts && args[i] == "--" {
            parsing_opts = false;
            i += 1;
            continue;
        }
        if parsing_opts && args[i] == "--cols" {
            i += 1;
            cols = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(80);
        } else if parsing_opts && args[i] == "--rows" {
            i += 1;
            rows = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(24);
        } else if session_name.is_empty() {
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

    // Daemonize: fork into background
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            // Parent: print info and exit
            println!(
                "{}",
                serde_json::json!({
                    "session": session_name,
                    "pid": child.as_raw(),
                    "socket": sock_path.to_string_lossy(),
                })
            );
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

    let session = Session::new(session_name, cmd, &str_args, cols, rows)?;
    let mut server = Server::new(&sess_dir, session)?;
    server.run()?;

    Ok(())
}

/// Recursively find all sessions under a directory.
/// Returns session names relative to the socket root directory.
fn find_sessions(base: &std::path::Path, prefix: &str) -> io::Result<Vec<String>> {
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

        // Skip the socket file itself
        if name == SOCKET_FILENAME {
            continue;
        }

        if path.is_dir() {
            let full_name = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            // Check if this directory has a socket (is a session)
            let sock = path.join(SOCKET_FILENAME);
            if sock.exists() {
                let meta = std::fs::metadata(&sock)?;
                if meta.file_type().is_socket() {
                    sessions.push(full_name.clone());
                }
            }

            // Recurse into subdirectories for child sessions
            let children = find_sessions(&path, &full_name)?;
            sessions.extend(children);
        }
    }

    Ok(sessions)
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
/// first non-option argument, where `--cols/--rows` consume their next value.
fn parse_session_name(args: &[String]) -> Option<&str> {
    let mut parsing_opts = true;
    let mut i = 0;
    while i < args.len() {
        if parsing_opts && args[i] == "--" {
            parsing_opts = false;
            i += 1;
            continue;
        }
        if parsing_opts && (args[i] == "--cols" || args[i] == "--rows") {
            i += 2;
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
    let name = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
    if !sock.exists() {
        eprintln!("Error: session '{}' not found", name);
        std::process::exit(1);
    }

    let exit_code = bridge::run(&sock)?;
    std::process::exit(exit_code);
}

fn cmd_open(args: &[String]) -> io::Result<()> {
    let name = parse_session_name(args).unwrap_or_else(|| {
        eprintln!("Error: session name required");
        std::process::exit(1);
    });

    let sock = session_socket_path(name);
    if !sock.exists() {
        cmd_new(args)?;
        let ok = wait_for_socket(&sock, Duration::from_millis(3000), Duration::from_millis(50))?;
        if !ok {
            eprintln!(
                "Error: session '{}' was created but socket did not appear in time",
                name
            );
            std::process::exit(1);
        }
    }

    let exit_code = bridge::run(&sock)?;
    std::process::exit(exit_code);
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
        "new" => cmd_new(&args[2..]),
        "attach" => cmd_attach(&args[2..]),
        "open" => cmd_open(&args[2..]),
        "list" | "ls" => cmd_list(&args[2..]),
        "kill" => cmd_kill(&args[2..]),
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
