use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

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
