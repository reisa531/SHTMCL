//! Real process behaviours: spawn with captured stdio, wait with a timeout,
//! liveness and a forceful stop. These tests start actual OS processes, so they
//! stay small and use commands every supported platform ships.

use std::time::Duration;

use shtmcl_platform::process::{ProcessRequest, ProcessSupervisor, WaitOutcome};
use shtmcl_platform::process_std::StdProcesses;
use shtmcl_platform::{PlatformDirs, ProcessLauncher};

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("shtmcl-process-tests");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A program that prints a line and exits with a known code.
fn exits_with(code: i32) -> ProcessRequest {
    if cfg!(windows) {
        // `exit` is a cmd builtin, so it needs the shell.
        ProcessRequest::new("cmd".into(), temp_dir())
            .with_args(vec!["/c".into(), format!("echo hello && exit {code}")])
    } else {
        ProcessRequest::new("/bin/sh".into(), temp_dir())
            .with_args(vec!["-c".into(), format!("echo hello; exit {code}")])
    }
}

/// A program that stays alive for roughly `seconds`.
fn stays_alive(seconds: u64) -> ProcessRequest {
    if cfg!(windows) {
        ProcessRequest::new("cmd".into(), temp_dir()).with_args(vec![
            "/c".into(),
            format!("ping -n {seconds} 127.0.0.1 > nul"),
        ])
    } else {
        ProcessRequest::new("/bin/sh".into(), temp_dir())
            .with_args(vec!["-c".into(), format!("sleep {seconds}")])
    }
}

#[test]
fn spawns_waits_and_reports_the_exit_code() {
    let processes = StdProcesses::capturing();
    let handle = processes.spawn(&exits_with(3)).expect("spawn");
    let outcome = processes.wait_for_exit(handle.pid, Some(Duration::from_secs(20)));
    assert_eq!(outcome, WaitOutcome::Exited(3));
    assert!(!processes.is_alive(handle.pid));
    // The exit is remembered, so a second wait answers instead of going Unknown.
    assert_eq!(
        processes.wait_for_exit(handle.pid, Some(Duration::from_millis(10))),
        WaitOutcome::Exited(3)
    );
}

#[test]
fn captured_streams_are_readable_by_the_owner() {
    let processes = StdProcesses::capturing();
    let handle = processes.spawn(&exits_with(0)).expect("spawn");

    let mut streams = processes.take_streams(handle.pid).expect("streams");
    let mut stdout = String::new();
    use std::io::Read;
    streams
        .stdout
        .read_to_string(&mut stdout)
        .expect("read stdout");
    // Draining before waiting avoids a full pipe blocking the child.
    let mut stderr = String::new();
    streams
        .stderr
        .read_to_string(&mut stderr)
        .expect("read stderr");

    assert!(stdout.contains("hello"), "stdout was {stdout:?}");
    assert!(stderr.is_empty(), "stderr was {stderr:?}");
    assert_eq!(
        processes.wait_for_exit(handle.pid, Some(Duration::from_secs(20))),
        WaitOutcome::Exited(0)
    );
    // A second take has nothing left to hand out.
    assert!(processes.take_streams(handle.pid).is_none());
}

#[test]
fn a_running_process_times_out_then_stops() {
    let processes = StdProcesses::capturing();
    let handle = processes.spawn(&stays_alive(3)).expect("spawn");

    assert!(processes.is_alive(handle.pid));
    assert_eq!(
        processes.wait_for_exit(handle.pid, Some(Duration::from_millis(120))),
        WaitOutcome::TimedOut,
        "a wait shorter than the process must report TimedOut, not a fake exit"
    );

    processes.request_stop(handle.pid).expect("stop");
    let outcome = processes.wait_for_exit(handle.pid, Some(Duration::from_secs(20)));
    match outcome {
        WaitOutcome::Exited(_) => {}
        other => panic!("expected the stopped process to exit, got {other:?}"),
    }
    assert!(!processes.is_alive(handle.pid));
}

#[test]
fn unknown_pids_are_reported_instead_of_guessed() {
    let processes = StdProcesses::capturing();
    let missing = 0x7ff0_0001;
    assert_eq!(
        processes.wait_for_exit(missing, Some(Duration::from_millis(10))),
        WaitOutcome::Unknown
    );
    assert!(!processes.is_alive(missing));
    assert!(processes.take_streams(missing).is_none());
    let error = processes.request_stop(missing).expect_err("stop unknown");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn a_missing_program_is_an_io_error_not_a_panic() {
    let processes = StdProcesses::capturing();
    let request = ProcessRequest::new(
        temp_dir().join("definitely-not-a-real-program-xyz"),
        temp_dir(),
    );
    let error = processes.spawn(&request).expect_err("spawn must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(processes.tracked(), 0);
}

#[test]
fn dirs_layout_is_reused_as_the_working_directory() {
    let dirs = PlatformDirs::under(std::env::temp_dir().join("shtmcl-process-dirs"));
    dirs.create_all().expect("create layout");
    let processes = StdProcesses::inheriting();
    // `inheriting` must not hand out streams but must still supervise.
    let request = ProcessRequest::new("cmd".into(), dirs.data.clone());
    let request = if cfg!(windows) {
        request.with_args(vec!["/c".into(), "exit 0".into()])
    } else {
        ProcessRequest::new("/bin/sh".into(), dirs.data.clone())
            .with_args(vec!["-c".into(), "exit 0".into()])
    };
    let handle = processes.spawn(&request).expect("spawn");
    assert!(processes.take_streams(handle.pid).is_none());
    assert_eq!(
        processes.wait_for_exit(handle.pid, Some(Duration::from_secs(20))),
        WaitOutcome::Exited(0)
    );
}
