use std::io::Read;
use std::process::{Child, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};

use super::CommandTrace;

#[derive(Clone, Default)]
pub(crate) struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn check(&self) -> Result<()> {
        check_load_cancelled(Some(self.flag()))
    }

    pub(crate) fn flag(&self) -> &AtomicBool {
        &self.0
    }
}

pub(crate) fn check_load_cancelled(cancelled: Option<&AtomicBool>) -> Result<()> {
    ensure!(
        cancelled.is_none_or(|cancelled| !cancelled.load(Ordering::Relaxed)),
        "cancelled Go workspace load"
    );
    Ok(())
}

pub(crate) fn command_output(
    command: &mut Command,
    trace: Option<&mut CommandTrace>,
    cancelled: Option<&AtomicBool>,
) -> Result<Output> {
    if cancelled.is_none() {
        return Ok(CommandTrace::output(command, trace)?);
    }
    let (status, stdout, stderr) = read_command(command, trace, cancelled, read_pipe)?;
    Ok(Output {
        status,
        stdout: stdout?,
        stderr,
    })
}

pub(super) fn read_command<T: Send + 'static>(
    command: &mut Command,
    mut trace: Option<&mut CommandTrace>,
    cancelled: Option<&AtomicBool>,
    read_stdout: impl FnOnce(ChildStdout) -> Result<T> + Send + 'static,
) -> Result<(ExitStatus, Result<T>, Vec<u8>)> {
    if let Err(error) = check_load_cancelled(cancelled) {
        if let Some(trace) = &mut trace {
            trace.cancelled();
        }
        return Err(error);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(trace) = &mut trace {
        trace.spawned(child.id());
    }
    let stdout = child.stdout.take().expect("Go command stdout is piped");
    let stderr = child.stderr.take().expect("Go command stderr is piped");
    let stderr = thread::spawn(move || read_pipe(stderr));
    let (status, stdout) = if let Some(cancelled) = cancelled {
        let stdout = thread::spawn(move || read_stdout(stdout));
        let status = wait_for_output(child, &stdout, &stderr, trace, cancelled)?;
        (status, join_reader(stdout)?)
    } else {
        let stdout = read_stdout(stdout);
        let status = child.wait().context("waiting for Go command")?;
        if let Some(trace) = trace {
            trace.exited(status);
        }
        (status, stdout)
    };
    let stderr = join_reader(stderr)?.context("reading Go command stderr")?;
    Ok((status, stdout, stderr))
}

fn wait_for_output<T>(
    mut child: Child,
    stdout: &JoinHandle<Result<T>>,
    stderr: &JoinHandle<Result<Vec<u8>>>,
    mut trace: Option<&mut CommandTrace>,
    cancelled: &AtomicBool,
) -> Result<ExitStatus> {
    let mut status = None;
    loop {
        if let Err(error) = check_load_cancelled(Some(cancelled)) {
            if let Some(trace) = &mut trace {
                trace.cancelled();
            }
            if status.is_none() {
                let _ = child.kill();
                thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            // A descendant may still hold the pipes after the launcher exits.
            // The readers can finish independently after its copies close.
            return Err(error);
        }
        if status.is_none() {
            status = child.try_wait().context("waiting for Go command")?;
            if let (Some(trace), Some(status)) = (&mut trace, status) {
                trace.exited(status);
            }
        }
        if let Some(status) = status
            && stdout.is_finished()
            && stderr.is_finished()
        {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn read_pipe(mut pipe: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader<T>(reader: JoinHandle<Result<T>>) -> Result<Result<T>> {
    reader
        .join()
        .map_err(|_| anyhow!("Go command reader panicked"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use crate::test_support::TestProject;

    use super::*;

    const MODE: &str = "RS_GOPLS_TEST_PIPE_MODE";
    const DIRECTORY: &str = "RS_GOPLS_TEST_PIPE_DIRECTORY";

    fn pipe_command(mode: &str, directory: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "workspace::process::tests::cancelled_command_releases_the_loading_slot",
                "--nocapture",
            ])
            .env(MODE, mode)
            .env(DIRECTORY, directory);
        command
    }

    #[test]
    fn cancelled_command_releases_the_loading_slot() {
        if let Ok(mode) = std::env::var(MODE) {
            let directory = std::path::PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
            match mode.as_str() {
                "launcher" => {
                    pipe_command("holder", &directory)
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .spawn()
                        .unwrap();
                }
                "holder" => {
                    std::fs::write(directory.join("ready"), "").unwrap();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !directory.join("release").is_file() && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                "quick" => {}
                _ => panic!("unexpected pipe command mode"),
            }
            return;
        }

        for launcher_exits in [false, true] {
            let project = TestProject::new();
            let mode = if launcher_exits { "launcher" } else { "holder" };
            let mut child = pipe_command(mode, &project.root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            if launcher_exits {
                assert!(child.wait().unwrap().success());
            }
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let stdout = thread::spawn(move || read_pipe(stdout));
            let stderr = thread::spawn(move || read_pipe(stderr));
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_cancelled = Arc::clone(&cancelled);
            let (done, result) = crossbeam_channel::bounded(1);
            let worker = thread::spawn(move || {
                let outcome = wait_for_output(child, &stdout, &stderr, None, &worker_cancelled);
                let _ = done.send(outcome);
            });

            let deadline = Instant::now() + Duration::from_secs(5);
            while !project.root.join("ready").is_file() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(project.root.join("ready").is_file());
            assert!(result.try_recv().is_err());
            cancelled.store(true, Ordering::Relaxed);
            let outcome = result.recv_timeout(Duration::from_secs(2));
            project.write("release", "");
            worker.join().unwrap();
            assert!(
                outcome
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("cancelled")
            );

            let fresh = AtomicBool::new(false);
            let output = command_output(
                &mut pipe_command("quick", &project.root),
                None,
                Some(&fresh),
            )
            .unwrap();
            assert!(output.status.success());
        }
    }
}
