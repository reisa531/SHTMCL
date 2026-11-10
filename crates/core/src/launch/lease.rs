//! Keep instance protection alive across preparation and process supervision.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use shtmcl_domain::ErrorEnvelope;

use super::ports::{InstanceLaunchLease, ProcessSupervisor};

struct State {
    lease: Box<dyn InstanceLaunchLease>,
    stop: bool,
    failure: Option<ErrorEnvelope>,
    pid: Option<u32>,
}

pub(super) struct LaunchLeaseGuard {
    state: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    supervisor: Arc<dyn ProcessSupervisor>,
}

impl LaunchLeaseGuard {
    pub(super) fn new(
        lease: Box<dyn InstanceLaunchLease>,
        supervisor: Arc<dyn ProcessSupervisor>,
    ) -> Result<Self, ErrorEnvelope> {
        Self::with_interval(lease, supervisor, Duration::from_secs(10))
    }

    fn with_interval(
        lease: Box<dyn InstanceLaunchLease>,
        supervisor: Arc<dyn ProcessSupervisor>,
        interval: Duration,
    ) -> Result<Self, ErrorEnvelope> {
        let state = Arc::new((
            Mutex::new(State {
                lease,
                stop: false,
                failure: None,
                pid: None,
            }),
            Condvar::new(),
        ));
        let shared = state.clone();
        let process = supervisor.clone();
        let worker = std::thread::Builder::new()
            .name("02-instance-lease".into())
            .spawn(move || {
                let (lock, wake) = &*shared;
                let mut current = lock.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    let (next, _) = wake
                        .wait_timeout_while(current, interval, |s| !s.stop)
                        .unwrap_or_else(|e| e.into_inner());
                    current = next;
                    if current.stop {
                        break;
                    }
                    if current.failure.is_none() {
                        if let Err(error) = current.lease.renew() {
                            current.failure = Some(error);
                        }
                    }
                    let failed_pid = current.failure.as_ref().and(current.pid);
                    if let Some(pid) = failed_pid {
                        // Keep the guard held until the exit watcher confirms exit.
                        drop(current);
                        let _ = process.request_stop(pid);
                        current = lock.lock().unwrap_or_else(|e| e.into_inner());
                    }
                }
            })
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        Ok(Self {
            state,
            worker: Some(worker),
            supervisor,
        })
    }

    /// A synchronous renewal closes the heartbeat-to-spawn expiry window.
    pub(super) fn check_before_spawn(&self) -> Result<(), ErrorEnvelope> {
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        let result = state.lease.renew();
        if let Err(error) = &result {
            state.failure = Some(error.clone());
        }
        result
    }

    pub(super) fn attach_process(&self, pid: u32) -> Result<(), ErrorEnvelope> {
        let result = {
            let mut state = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
            state.pid = Some(pid);
            let result = state.lease.bind_process(pid);
            if let Err(error) = result {
                state.failure.get_or_insert(error);
            }
            state.failure.clone().map_or(Ok(()), Err)
        };
        if result.is_err() {
            let _ = self.supervisor.request_stop(pid);
        }
        result
    }
}

impl Drop for LaunchLeaseGuard {
    fn drop(&mut self) {
        {
            let mut state = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
            state.stop = true;
            self.state.1.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // The final State owner now drops the adapter, releasing its lease.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::ports::{ChildStreams, WaitOutcome};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    struct Lease {
        released: Arc<AtomicBool>,
        renewals: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }
    impl InstanceLaunchLease for Lease {
        fn renew(&mut self) -> Result<(), ErrorEnvelope> {
            self.renewals.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(ErrorEnvelope::module_unavailable())
            } else {
                Ok(())
            }
        }
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }
    #[derive(Default)]
    struct Process {
        stops: AtomicUsize,
    }
    impl ProcessSupervisor for Process {
        fn take_streams(&self, _: u32) -> Option<ChildStreams> {
            None
        }
        fn wait_for_exit(&self, _: u32, _: Option<Duration>) -> WaitOutcome {
            WaitOutcome::TimedOut
        }
        fn is_alive(&self, _: u32) -> bool {
            true
        }
        fn request_stop(&self, _: u32) -> std::io::Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn heartbeat_renews_during_preparation_and_keeps_failed_running_guard() {
        let released = Arc::new(AtomicBool::new(false));
        let renewals = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let process = Arc::new(Process::default());
        let guard = LaunchLeaseGuard::with_interval(
            Box::new(Lease {
                released: released.clone(),
                renewals: renewals.clone(),
                fail: fail.clone(),
            }),
            process.clone(),
            Duration::from_millis(5),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while renewals.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            renewals.load(Ordering::SeqCst) >= 2,
            "preparation renews without process polling"
        );
        guard.attach_process(42).unwrap();
        fail.store(true, Ordering::SeqCst);
        while process.stops.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(process.stops.load(Ordering::SeqCst) > 0);
        assert!(guard.check_before_spawn().is_err());
        assert!(
            !released.load(Ordering::SeqCst),
            "stop request is not proof of exit"
        );
        drop(guard);
        assert!(released.load(Ordering::SeqCst));
    }
}
