//! Side effects the hypervisor performs for the kernel image: `bash` (in a
//! mount-namespace sandbox where protected paths are read-only), file
//! writes with the protected-path rules, and path resolution.
//!
//! The sandbox is `unshare --user --map-root-user --mount`: inside it, each
//! protected path is bind-mounted read-only over itself, then the command
//! runs. If user namespaces are not available, commands run without it and
//! the hypervisor says so once on stderr.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

pub const OUTPUT_MAX: usize = 20 * 1024;

pub struct BashResult {
    pub output: String,
    /// -1 when killed.
    pub exit_code: i32,
    pub timed_out: bool,
}

/// Mounts each path in `$CMH_RO` read-only and each in `$CMH_TMPFS` as an
/// empty tmpfs, then runs `$CMH_CMD`.
const SANDBOX_SCRIPT: &str = r#"IFS='
'
for p in $CMH_RO; do [ -e "$p" ] && mount --bind -o ro "$p" "$p"; done
for p in $CMH_TMPFS; do [ -d "$p" ] && mount -t tmpfs -o size=1m tmpfs "$p"; done
c=$CMH_CMD
unset CMH_RO CMH_TMPFS CMH_CMD IFS
exec bash -c "$c""#;

/// True if `unshare` can make a user + mount namespace here.
pub fn sandbox_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let ok = Command::new("unshare")
            .args(["-r", "-m", "--propagation", "private", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("[hv] warning: user namespaces unavailable; bash and image builds run without the read-only sandbox");
        }
        ok
    })
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n")
}

/// A `bash -c cmd` command, inside the sandbox if it is available.
/// `net` = false also removes network access (image builds).
pub fn sandboxed(cmd: &str, read_only: &[PathBuf], tmpfs: &[PathBuf], net: bool) -> Command {
    if !sandbox_available() {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        return c;
    }
    let mut c = Command::new("unshare");
    c.args(["-r", "-m", "--propagation", "private"]);
    if !net {
        c.arg("-n");
    }
    c.args(["--", "bash", "-c", SANDBOX_SCRIPT])
        .env("CMH_RO", join_paths(read_only))
        .env("CMH_TMPFS", join_paths(tmpfs))
        .env("CMH_CMD", cmd);
    c
}

/// Runs `cmd` with a timeout; output (stdout+stderr) is capped at `cap` bytes.
pub fn run_capped(mut cmd: Command, work_dir: &Path, timeout: Duration, cap: usize) -> Result<BashResult, String> {
    let (mut reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let writer2 = writer.try_clone().map_err(|e| e.to_string())?;
    let mut child = cmd
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer2)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    drop(cmd);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            // Keep at most a little more than the cap; the rest is drained
            // so the child never blocks on a full pipe.
            let mut b = sink.lock().unwrap_or_else(|e| e.into_inner());
            if b.len() <= cap {
                b.extend_from_slice(&chunk[..n]);
            }
        }
        let _ = done_tx.send(());
    });
    let kill_group = |pid: u32| {
        let _ = Command::new("kill").args(["-9", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    // Background children may hold the pipe open; do not wait for them forever.
    if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        kill_group(child.id());
        let _ = done_rx.recv_timeout(Duration::from_secs(1));
    }
    let out = String::from_utf8_lossy(&buf.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    Ok(BashResult { output: truncate(&out, cap), exit_code: status.and_then(|s| s.code()).unwrap_or(-1), timed_out: status.is_none() })
}

/// The `bash` capability: the work dir, a timeout, an output cap, and the
/// protected paths read-only.
pub fn bash(work_dir: &Path, command: &str, timeout_s: u32, protected: &[PathBuf]) -> Result<BashResult, String> {
    let timeout = Duration::from_secs(u64::from(timeout_s.clamp(1, 3600)));
    run_capped(sandboxed(command, protected, &[], true), work_dir, timeout, OUTPUT_MAX)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... (truncated, {} bytes total)", &s[..cut], s.len())
}

/// Absolute, lexically normalised path with the existing prefix canonicalised.
pub fn resolve(work_dir: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { work_dir.join(p) };
    let mut norm = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    let mut existing = norm.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return norm,
        }
    }
    let mut out = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
    for part in rest.iter().rev() {
        out.push(part);
    }
    out
}

/// The protected path containing `path`, if any (both resolved).
pub fn protected_by<'a>(path: &Path, protected: &'a [PathBuf]) -> Option<&'a PathBuf> {
    protected.iter().find(|p| path.starts_with(p))
}

/// The `write-file` capability.
pub fn write_file(work_dir: &Path, path: &str, content: &str, protected: &[PathBuf]) -> Result<String, String> {
    let path = resolve(work_dir, path);
    if let Some(p) = protected_by(&path, protected) {
        return Err(format!("refused: {} is inside protected path {}", path.display(), p.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
}
