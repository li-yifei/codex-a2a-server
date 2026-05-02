use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
pub fn configure_child_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(windows)]
pub fn configure_child_process(_command: &mut Command) {}

#[cfg(unix)]
pub fn terminate_process_tree(pid: u32) -> Result<(), String> {
    let process_group = format!("-{pid}");
    Command::new("kill")
        .arg("-TERM")
        .arg("--")
        .arg(&process_group)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    thread::sleep(Duration::from_millis(150));
    Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(process_group)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(windows)]
pub fn terminate_process_tree(pid: u32) -> Result<(), String> {
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/T")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(150));

    // Windows has no direct SIGTERM equivalent for arbitrary child console
    // processes, so cancellation falls back to forced tree termination.
    let status = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/T")
        .arg("/F")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("taskkill failed for pid {pid}: {status}"))
    }
}
