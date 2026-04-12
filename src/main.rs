mod bridge;
mod constants;
mod paths;
mod pty;
mod server;
mod session;

use crate::paths::{find_sessions, session_dir, session_socket_path, socket_dir, SOCKET_FILENAME};
use server::Server;
use session::Session;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Return true when `cmd` resolves to the zsh executable.
fn is_zsh(cmd: &str) -> bool {
    Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "zsh")
        .unwrap_or(false)
}

/// Create a ZDOTDIR shim directory for the given session.
///
/// The shim intercepts `.zshenv` and `.zshrc` loading transparently:
///
/// - `.zshenv` forwards to the user's original `.zshenv`
/// - `.zshrc` unsets ZDOTDIR (so child shells behave normally), forwards to
///   the user's original `.zshrc`, then installs a `precmd` hook that sources
///   `$PTERM_ENV_FILE` before each prompt.
///
/// Returns the path to the created zdotdir directory.
fn setup_zsh_zdotdir(sess_dir: &Path) -> io::Result<std::path::PathBuf> {
    let zdotdir = sess_dir.join("zdotdir");
    std::fs::create_dir_all(&zdotdir)?;

    // Resolve the user's original dotfile directory (ZDOTDIR or HOME).
    let orig = std::env::var("ZDOTDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| "~".to_string());

    // Single-quote a path for safe shell embedding.
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let orig_q = q(&orig);

    let zshenv = format!(
        "# pterm: forward to original .zshenv\n\
         [[ -f {orig}/.zshenv ]] && builtin source {orig}/.zshenv\n",
        orig = orig_q,
    );

    let zshrc = format!(
        "# pterm: restore ZDOTDIR for child shells and forward to original .zshrc\n\
         unset ZDOTDIR\n\
         [[ -f {orig}/.zshrc ]] && builtin source {orig}/.zshrc\n\
         # Install pterm env-sync hook (no-op when PTERM_ENV_FILE is unset)\n\
         if [[ -n ${{PTERM_ENV_FILE:-}} ]]; then\n\
           autoload -Uz add-zsh-hook 2>/dev/null\n\
           _pterm_precmd() {{ [[ -r $PTERM_ENV_FILE ]] && builtin source $PTERM_ENV_FILE; }}\n\
           add-zsh-hook precmd _pterm_precmd\n\
         fi\n",
        orig = orig_q,
    );

    std::fs::write(zdotdir.join(".zshenv"), zshenv.as_bytes())?;
    std::fs::write(zdotdir.join(".zshrc"), zshrc.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in &[".zshenv", ".zshrc"] {
            std::fs::set_permissions(zdotdir.join(name), std::fs::Permissions::from_mode(0o600))?;
        }
    }

    Ok(zdotdir)
}

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

    // Always inject PTERM_ENV_FILE so the shell knows where Neovim's env
    // overrides are written on each attach.
    let env_file = sess_dir.join("env.sh");
    let env_file_str = env_file.to_string_lossy().into_owned();

    // For zsh: create a ZDOTDIR shim that installs the precmd hook
    // automatically.  Users do not need to modify their .zshrc.
    let zdotdir_str: Option<String> = if is_zsh(cmd) {
        match setup_zsh_zdotdir(&sess_dir) {
            Ok(p) => Some(p.to_string_lossy().into_owned()),
            Err(e) => {
                log::warn!("Failed to set up zsh ZDOTDIR shim: {}", e);
                None
            }
        }
    } else {
        None
    };

    let mut extra_env: Vec<(&str, &str)> = vec![("PTERM_ENV_FILE", &env_file_str)];
    if let Some(ref zd) = zdotdir_str {
        extra_env.push(("ZDOTDIR", zd.as_str()));
    }

    let session = Session::new(session_name, cmd, &str_args, &extra_env)?;
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
