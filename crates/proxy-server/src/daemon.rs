//! `--daemon`: detach from the terminal and keep running in the background.
//!
//! The working directory is deliberately left alone: `config.toml`, `data/` and
//! `captures/` are all resolved relative to where the process was launched, so
//! a chdir would silently break every one of them.

#[cfg(unix)]
pub const LOG_PATH: &str = "data/cc-proxy.log";
#[cfg(unix)]
pub const PID_PATH: &str = "data/cc-proxy.pid";

#[cfg(unix)]
pub fn daemonize() -> anyhow::Result<()> {
    // Checked before forking so the error is still visible on the terminal.
    refuse_if_already_running()?;
    fork_parent_exits()?;
    // A new session detaches from the controlling terminal, so closing the
    // shell no longer delivers SIGHUP.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    // Second fork: the child is not a session leader, so it can never reacquire
    // a controlling terminal.
    fork_parent_exits()?;

    redirect_stdio(std::path::Path::new(LOG_PATH))?;
    write_pid(std::path::Path::new(PID_PATH))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn daemonize() -> anyhow::Result<()> {
    anyhow::bail!("-d/--daemon is only supported on Unix")
}

#[cfg(unix)]
fn fork_parent_exits() -> anyhow::Result<()> {
    match unsafe { libc::fork() } {
        -1 => Err(std::io::Error::last_os_error().into()),
        0 => Ok(()),
        // `_exit` rather than `exit` so the parent neither runs destructors nor
        // flushes buffers it shares with the child.
        _ => unsafe { libc::_exit(0) },
    }
}

#[cfg(unix)]
fn redirect_stdio(log_path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::io::AsRawFd;

    use std::fs::{File, OpenOptions};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // ManuallyDrop: these fds are handed to 0/1/2 via dup2, so they must not be
    // closed when the values go out of scope.
    let log = std::mem::ManuallyDrop::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?,
    );
    let devnull = std::mem::ManuallyDrop::new(File::open("/dev/null")?);
    unsafe {
        libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(())
}

#[cfg(unix)]
fn write_pid(pid_path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(pid_path, format!("{}\n", std::process::id()))?;
    Ok(())
}

/// Refuse to spawn a second daemon while the recorded pid is still alive.
/// A stale pidfile is simply overwritten.
#[cfg(unix)]
fn refuse_if_already_running() -> anyhow::Result<()> {
    let Ok(text) = std::fs::read_to_string(PID_PATH) else {
        return Ok(());
    };
    let Some(pid) = text.trim().parse::<u32>().ok() else {
        return Ok(());
    };
    if std::path::Path::new(&format!("/proc/{pid}")).exists() {
        anyhow::bail!(
            "already running (pid {pid}, see {PID_PATH}); stop it first, or delete the pidfile"
        );
    }
    Ok(())
}
