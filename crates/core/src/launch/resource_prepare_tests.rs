//! Resource preparation must remain inside the launch lease and precede the
//! complete snapshot, which may legitimately fail until resources are restored.
use super::*;
use crate::launch::ports::{
    InstanceLaunchLease, InstanceLaunchLeaseSource, InstanceSnapshotSource, LaunchResourcePreparer,
};
use shtmcl_domain::{CancelTaskRequest, OperationId};
use std::sync::atomic::AtomicBool;

#[derive(Default)]
struct Trace {
    calls: Mutex<Vec<String>>,
    held: AtomicBool,
    holder: Mutex<String>,
}

impl Trace {
    fn push(&self, call: &str) {
        self.calls.lock().unwrap().push(call.to_string());
    }
}

struct Leases(Arc<Trace>);
struct Lease(Arc<Trace>);

impl InstanceLaunchLeaseSource for Leases {
    fn acquire(
        &self,
        _instance: &InstanceId,
        holder: &str,
        _operation: &OperationId,
    ) -> Result<Box<dyn InstanceLaunchLease>, ErrorEnvelope> {
        assert!(!self.0.held.swap(true, Ordering::SeqCst));
        *self.0.holder.lock().unwrap() = holder.to_string();
        self.0.push("acquire");
        Ok(Box::new(Lease(self.0.clone())))
    }
}

impl InstanceLaunchLease for Lease {
    fn renew(&mut self) -> Result<(), ErrorEnvelope> {
        assert!(self.0.held.load(Ordering::SeqCst));
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        assert!(self.0.held.swap(false, Ordering::SeqCst));
        self.0.push("release");
    }
}

struct Snapshots(Arc<Trace>);

impl InstanceSnapshotSource for Snapshots {
    fn snapshot(
        &self,
        _instance: &InstanceId,
    ) -> Result<plan::InstanceLaunchSnapshot, ErrorEnvelope> {
        panic!("the full snapshot must be requested under the lease")
    }

    fn snapshot_under_lease(
        &self,
        _instance: &InstanceId,
        holder: &str,
    ) -> Result<plan::InstanceLaunchSnapshot, ErrorEnvelope> {
        assert!(self.0.held.load(Ordering::SeqCst));
        assert_eq!(holder, self.0.holder.lock().unwrap().as_str());
        self.0.push("snapshot");
        // A deterministic stop after proving the resource/snapshot ordering.
        Err(errors::instance_unavailable())
    }
}

enum Outcome {
    Complete,
    Fail,
    Cancel,
}

struct Preparer {
    trace: Arc<Trace>,
    tasks: Arc<dyn TaskService>,
    outcome: Outcome,
}

impl LaunchResourcePreparer for Preparer {
    fn prepare(
        &self,
        instance: &InstanceId,
        holder: &str,
        revision: Option<Revision>,
        task: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<(), ErrorEnvelope> {
        assert_eq!(instance.as_str(), "resource-instance");
        assert_eq!(revision, Some(Revision::new(7)));
        assert!(self.trace.held.load(Ordering::SeqCst));
        assert_eq!(holder, self.trace.holder.lock().unwrap().as_str());
        let snapshot = self.tasks.snapshot(task).unwrap();
        assert_eq!(snapshot.attempt_generation, generation);
        assert_eq!(snapshot.phase.as_str(), phases::RESOURCE_PREPARE);
        self.trace.push("prepare");
        match self.outcome {
            Outcome::Complete => Ok(()),
            Outcome::Fail => Err(ErrorEnvelope::module_unavailable()),
            Outcome::Cancel => {
                self.tasks.request_cancel(&CancelTaskRequest {
                    task_id: task.clone(),
                    attempt_generation: generation,
                });
                // Simulate an adapter returning an error while acknowledging
                // cancellation. The launch must report cancellation, not failure.
                Err(ErrorEnvelope::module_unavailable())
            }
        }
    }
}

struct NoSpawn(Arc<AtomicU64>);
impl shtmcl_platform::process::ProcessLauncher for NoSpawn {
    fn spawn(
        &self,
        _request: &shtmcl_platform::process::ProcessRequest,
    ) -> std::io::Result<shtmcl_platform::process::ProcessHandle> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("resource preparation refusal must not spawn")
    }
}

struct CancelAtPreparation(Arc<dyn TaskService>);
impl ports::StepObserver for CancelAtPreparation {
    fn on_phase(&self, phase: &'static str) {
        if phase == phases::RESOURCE_PREPARE {
            let task = self.0.snapshots().pop().unwrap();
            self.0.request_cancel(&CancelTaskRequest {
                task_id: task.id,
                attempt_generation: task.attempt_generation,
            });
        }
    }
}

fn run(outcome: Option<Outcome>) -> (Arc<Trace>, shtmcl_domain::Task, LaunchSession) {
    run_case(outcome, false)
}

fn run_case(
    outcome: Option<Outcome>,
    cancel_before_preparation: bool,
) -> (Arc<Trace>, shtmcl_domain::Task, LaunchSession) {
    let app = crate::CoreApp::new(shtmcl_platform::PlatformDirs::under(
        std::env::temp_dir().join("shtmcl-resource-port-unit"),
    ));
    let ctx = app.context();
    let trace = Arc::new(Trace::default());
    let spawns = Arc::new(AtomicU64::new(0));
    let mut ports = LaunchPorts::unavailable()
        .with_instances(Arc::new(Snapshots(trace.clone())))
        .with_leases(Arc::new(Leases(trace.clone())));
    ports.launcher = Arc::new(NoSpawn(spawns.clone()));
    if cancel_before_preparation {
        ports = ports.with_steps(Arc::new(CancelAtPreparation(ctx.tasks().clone())));
    }
    if let Some(outcome) = outcome {
        ports = ports.with_resource_preparer(Arc::new(Preparer {
            trace: trace.clone(),
            tasks: ctx.tasks().clone(),
            outcome,
        }));
    }
    let launch = LaunchModule::new(ctx.clone(), ports);
    let response = launch
        .handle(&CommandRequest {
            request_id: shtmcl_domain::RequestId::new("resource-test").unwrap(),
            operation_id: Some(OperationId::new("resource-operation").unwrap()),
            entity_id: None,
            expected_revision: Some(Revision::new(7)),
            payload: serde_json::json!({
                "command": commands::LAUNCH_PREPARE,
                "instanceId": "resource-instance"
            }),
        })
        .unwrap();
    let CommandResponse::Accepted { task_id, .. } = response else {
        panic!("launch must be asynchronous");
    };
    launch.join_jobs();
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    assert!(!trace.held.load(Ordering::SeqCst));
    let session = launch.sessions().pop().unwrap();
    (trace, ctx.tasks().snapshot(&task_id).unwrap(), session)
}

#[test]
fn resource_preparer_uses_same_holder_before_complete_snapshot() {
    let (trace, _, _) = run(Some(Outcome::Complete));
    assert_eq!(
        *trace.calls.lock().unwrap(),
        ["acquire", "prepare", "snapshot", "release"]
    );
}

#[test]
fn resource_failure_never_reads_full_snapshot_or_spawns_and_releases_lease() {
    let (trace, task, session) = run(Some(Outcome::Fail));
    assert_eq!(
        *trace.calls.lock().unwrap(),
        ["acquire", "prepare", "release"]
    );
    assert_eq!(task.state, TaskState::Failed);
    assert!(session.has_evidence("failed:resource_prepare"));
}

#[test]
fn cancelled_resource_preparation_never_spawns_and_releases_lease() {
    let (trace, task, session) = run(Some(Outcome::Cancel));
    assert_eq!(
        *trace.calls.lock().unwrap(),
        ["acquire", "prepare", "release"]
    );
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(session.state, LaunchSessionState::Cancelled);
    assert!(session.has_evidence(evidence::SAFE_STOP_REACHED));
}

#[test]
fn unconfigured_preparer_keeps_existing_snapshot_flow() {
    let (trace, _, _) = run(None);
    assert_eq!(
        *trace.calls.lock().unwrap(),
        ["acquire", "snapshot", "release"]
    );
}

#[test]
fn cancellation_at_resource_phase_skips_preparer_and_releases_lease() {
    let (trace, task, _) = run_case(Some(Outcome::Complete), true);
    assert_eq!(*trace.calls.lock().unwrap(), ["acquire", "release"]);
    assert_eq!(task.state, TaskState::Cancelled);
}
