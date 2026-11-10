//! One job at a time: the work on its own thread, its progress for the page while it
//! runs, its result kept until the next job replaces it, a cancel flag the work
//! checks as it goes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use crate::analyse::Progress;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Failed,
    Cancelled,
}

struct Job {
    id: u64,
    kind: String,
    path: String,
    state: State,
    started: Instant,
    finished: Option<Instant>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    result: Option<serde_json::Value>,
    error: Option<String>,
}

#[derive(Default)]
pub struct Jobs {
    current: Mutex<Option<Job>>,
    next: AtomicU64,
}

/// Another job is running.
#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

impl Jobs {
    pub fn new() -> Jobs {
        Jobs::default()
    }

    /// The work started on a thread; its id, or Busy while another runs.
    pub fn start<F>(self: &Arc<Self>, kind: &str, path: &str, work: F) -> Result<u64, Busy>
    where
        F: FnOnce(&Progress, &AtomicBool) -> Result<serde_json::Value, String> + Send + 'static,
    {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(&*current, Some(job) if job.state == State::Running) {
            return Err(Busy);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        *current = Some(Job {
            id,
            kind: kind.to_string(),
            path: path.to_string(),
            state: State::Running,
            started: Instant::now(),
            finished: None,
            progress: progress.clone(),
            cancel: cancel.clone(),
            result: None,
            error: None,
        });
        drop(current);
        let jobs = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("job-{id}"))
            .spawn(move || {
                let outcome = work(&progress, &cancel);
                let mut current = jobs.current.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(job) = current.as_mut().filter(|j| j.id == id) {
                    job.finished = Some(Instant::now());
                    match outcome {
                        Ok(value) => {
                            job.state = if cancel.load(Ordering::Relaxed) {
                                State::Cancelled
                            } else {
                                State::Done
                            };
                            job.result = Some(value);
                        }
                        Err(e) => {
                            job.state = State::Failed;
                            job.error = Some(e);
                        }
                    }
                }
            });
        if spawned.is_err() {
            let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(job) = current.as_mut().filter(|j| j.id == id) {
                job.state = State::Failed;
                job.error = Some("the job's thread could not be started".to_string());
                job.finished = Some(Instant::now());
            }
        }
        Ok(id)
    }

    /// The job as the page reads it, none for an id that is not the current one.
    pub fn get(&self, id: u64) -> Option<serde_json::Value> {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        current.as_ref().filter(|j| j.id == id).map(view)
    }

    /// The current job, running or ended, for a page that opens after it was started.
    pub fn current(&self) -> Option<serde_json::Value> {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        current.as_ref().map(view)
    }

    /// The running job told to stop; false when there is no such running job.
    pub fn cancel(&self, id: u64) -> bool {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        match current.as_ref() {
            Some(job) if job.id == id && job.state == State::Running => {
                job.cancel.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
}

/// A job as the page reads it.
fn view(job: &Job) -> serde_json::Value {
    let end = job.finished.unwrap_or_else(Instant::now);
    serde_json::json!({
        "id": job.id,
        "kind": job.kind,
        "path": job.path,
        "state": job.state,
        "directories": job.progress.directories.load(Ordering::Relaxed),
        "files": job.progress.files.load(Ordering::Relaxed),
        "seconds": end.duration_since(job.started).as_secs(),
        "result": job.result,
        "error": job.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn wait_until_not_running(jobs: &Jobs, id: u64) -> serde_json::Value {
        for _ in 0..200 {
            let job = jobs.get(id).unwrap();
            if job["state"] != "running" {
                return job;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the job did not end");
    }

    #[test]
    fn a_job_runs_reports_its_progress_and_keeps_its_result() {
        let jobs = Arc::new(Jobs::new());
        let (tx, rx) = mpsc::channel::<()>();
        let id = jobs
            .start("ignore", "NAS", move |progress, _| {
                progress.directories.fetch_add(3, Ordering::Relaxed);
                rx.recv().ok();
                Ok(serde_json::json!({ "answer": 42 }))
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let job = jobs.get(id).unwrap();
        assert_eq!(job["state"], "running");
        assert_eq!(job["directories"], 3);
        assert_eq!(job["kind"], "ignore");
        assert_eq!(job["path"], "NAS");
        assert_eq!(
            jobs.start("ignore", "USB", |_, _| Ok(serde_json::Value::Null)),
            Err(Busy)
        );
        tx.send(()).unwrap();
        let job = wait_until_not_running(&jobs, id);
        assert_eq!(job["state"], "done");
        assert_eq!(job["result"]["answer"], 42);
        assert!(jobs.get(id + 1).is_none());
        assert_eq!(jobs.current().unwrap()["id"], id);
        // Done: the next job replaces it.
        let next = jobs
            .start("ignore", "USB", |_, _| Err("no".to_string()))
            .unwrap();
        assert_eq!(next, id + 1);
        let job = wait_until_not_running(&jobs, next);
        assert_eq!(job["state"], "failed");
        assert_eq!(job["error"], "no");
        assert!(jobs.get(id).is_none());
    }

    #[test]
    fn a_cancel_reaches_the_work_and_marks_the_job() {
        let jobs = Arc::new(Jobs::new());
        let id = jobs
            .start("ignore", "NAS", |_, cancel| {
                while !cancel.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(serde_json::json!({ "partial": true }))
            })
            .unwrap();
        assert!(jobs.cancel(id));
        let job = wait_until_not_running(&jobs, id);
        assert_eq!(job["state"], "cancelled");
        assert_eq!(job["result"]["partial"], true);
        assert!(!jobs.cancel(id));
        assert!(!jobs.cancel(99));
    }
}
