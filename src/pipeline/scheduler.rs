//! Per-group serialisation with a bounded global concurrency.
//!
//! Invariants enforced here:
//!
//! * **One worker per group** — two batches from the same group never run at the
//!   same time, so the Agy conversation for that group is advanced strictly in
//!   arrival order (FIFO).
//! * **Bounded global parallelism** — at most `agent_concurrency` batches run at
//!   once across all groups.
//! * **No silent loss** — a job is only removed from its lane after the executor
//!   returns; a failed job stays accounted for by the caller's own store record.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};

use crate::pipeline::batcher::BatchLimits;

/// One unit of work handed to the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchJob {
    pub group_id: String,
    pub batch_id: String,
    /// Ids of the `pending_events` rows claimed by this batch.
    pub event_ids: Vec<i64>,
    /// Whether this batch was flushed because a privileged message arrived.
    pub privileged: bool,
    /// Total plain-text characters across `event_ids`, for merge accounting.
    pub chars: usize,
    /// Total image segments across `event_ids`, for merge accounting.
    pub images: usize,
    /// The pool this batch was claimed from had flooded and been drained:
    /// older messages were consumed as background, so the prompt tells the
    /// model to answer the live tail instead of working through everything
    /// in its context window as if it were all awaiting a reply.
    pub flooded: bool,
    /// Flood stage 4: the drained overflow compressed into a short
    /// background brief (the hy3 summary, or the script-truncated rendering
    /// when the summary failed, timed out or was never configured). Rides
    /// only the first job claimed after a drain; `None` on ordinary jobs.
    pub flood_background: Option<String>,
    /// One-shot replay of a batch whose turn died with no submission
    /// (2026-10-29): the intake claimed exactly the released event ids from
    /// the executor's retry mark, nothing else folded in. A retry job's own
    /// no-submission death never arms another replay.
    pub retry: bool,
}

/// Result of running one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecOutcome {
    /// The agent replied (or deliberately chose not to).
    Done,
    /// The call failed in a way that is safe to retry later; the batch is kept by
    /// the executor and the lane continues with the next job.
    Deferred,
    /// The conversation state is unknown (agent timeout); the lane must stop so
    /// the same batch is never replayed into the same session.
    Halted,
}

/// Something that can execute a batch. Implemented by the real pipeline and by the
/// test fakes.
#[async_trait::async_trait]
pub trait BatchExecutor: Send + Sync + 'static {
    async fn execute(&self, job: BatchJob) -> ExecOutcome;
}

/// Scheduling state for one group.
#[derive(Debug, Default)]
struct Lane {
    queue: VecDeque<BatchJob>,
    running: bool,
    /// Set when a run returned [`ExecOutcome::Halted`]; further enqueues are held
    /// until an operator clears it (the group is paused anyway).
    halted: bool,
}

/// Snapshot for the health / admin view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSnapshot {
    pub group_id: String,
    pub queued: usize,
    pub running: bool,
    pub halted: bool,
}

/// FIFO scheduler with a global concurrency limit.
pub struct Scheduler {
    lanes: Arc<Mutex<HashMap<String, Lane>>>,
    permits: Arc<Semaphore>,
    concurrency: usize,
    /// Caps a merged batch must stay within; the same limits the batcher cuts by.
    merge_limits: BatchLimits,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}

impl Scheduler {
    pub fn new(concurrency: usize, merge_limits: BatchLimits) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            permits: Arc::new(Semaphore::new(concurrency)),
            concurrency,
            merge_limits,
        }
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Queue a job for `group_id` and make sure a worker is running for that lane.
    ///
    /// Returns `false` when the lane is halted (a previous call returned
    /// [`ExecOutcome::Halted`]) — the caller must not treat the batch as accepted.
    pub async fn enqueue<E: BatchExecutor>(&self, executor: Arc<E>, job: BatchJob) -> bool {
        let group_id = job.group_id.clone();
        let start_worker = {
            let mut lanes = self.lanes.lock().await;
            let lane = lanes.entry(group_id.clone()).or_default();
            if lane.halted {
                lane.queue.push_back(job);
                return false;
            }
            // Merge instead of queueing a second batch behind a running turn: a job
            // still sitting in the queue has not been seen by the agent, and its rows
            // are still in `pending_events`, so folding the new batch into it costs
            // nothing and kills the queueing multiplier that made replies answer
            // minutes-old context. This happens under the same lane lock the worker
            // pops with, so a job can never be merged into after it was taken.
            // A non-empty queue implies the worker is running (the halted case is
            // handled above), so the merged job is guaranteed to be picked up.
            if lane.running
                && let Some(tail) = lane.queue.back_mut()
                && tail.event_ids.len() + job.event_ids.len() <= self.merge_limits.max_messages
                && tail.chars + job.chars <= self.merge_limits.max_chars
                && tail.images + job.images <= self.merge_limits.max_images
            {
                for id in &job.event_ids {
                    if !tail.event_ids.contains(id) {
                        tail.event_ids.push(*id);
                    }
                }
                tail.chars += job.chars;
                tail.images += job.images;
                tail.privileged |= job.privileged;
                return true;
            }
            lane.queue.push_back(job);
            if lane.running {
                false
            } else {
                lane.running = true;
                true
            }
        };

        if start_worker {
            let lanes = Arc::clone(&self.lanes);
            let permits = Arc::clone(&self.permits);
            tokio::spawn(async move {
                loop {
                    let job = {
                        let mut guard = lanes.lock().await;
                        let lane = match guard.get_mut(&group_id) {
                            Some(l) => l,
                            None => break,
                        };
                        match lane.queue.pop_front() {
                            Some(job) => job,
                            None => {
                                lane.running = false;
                                break;
                            }
                        }
                    };

                    let permit = match permits.acquire().await {
                        Ok(p) => p,
                        Err(_) => break,
                    };
                    let outcome = executor.execute(job.clone()).await;
                    drop(permit);

                    let mut guard = lanes.lock().await;
                    if let Some(lane) = guard.get_mut(&group_id)
                        && outcome == ExecOutcome::Halted
                    {
                        // Halting keeps the lane blocked but drops nothing here; the
                        // queued jobs were already discarded by `resume_lane`.
                        lane.halted = true;
                        lane.running = false;
                        break;
                    }
                }
            });
        }
        true
    }

    /// Current lane state, sorted by group id for stable rendering.
    pub async fn snapshot(&self) -> Vec<LaneSnapshot> {
        let lanes = self.lanes.lock().await;
        let mut out: Vec<LaneSnapshot> = lanes
            .iter()
            .map(|(group_id, lane)| LaneSnapshot {
                group_id: group_id.clone(),
                queued: lane.queue.len(),
                running: lane.running,
                halted: lane.halted,
            })
            .collect();
        out.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        out
    }

    /// Clear the halted flag for a group after an operator resolved the unknown
    /// result (session reset). Queued jobs are dropped deliberately: the design
    /// forbids replaying them into the same session. The dropped jobs are
    /// returned so the caller can release their claimed rows back to the
    /// pending pool (by event id — a merged job's rows keep several batch
    /// tags), where intake re-claims them as a fresh batch for the session
    /// that replaced the unknown one.
    pub async fn resume_lane(&self, group_id: &str) -> Vec<BatchJob> {
        let mut lanes = self.lanes.lock().await;
        match lanes.get_mut(group_id) {
            Some(lane) => {
                lane.halted = false;
                lane.running = false;
                lane.queue.drain(..).collect()
            }
            None => Vec::new(),
        }
    }

    /// Number of jobs waiting in one lane.
    pub async fn queued(&self, group_id: &str) -> usize {
        let lanes = self.lanes.lock().await;
        lanes.get(group_id).map(|l| l.queue.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Records ordering and peak concurrency.
    struct FakeExecutor {
        log: StdMutex<Vec<String>>,
        active: AtomicUsize,
        peak: AtomicUsize,
        hold: Notify,
        /// Per-group gate so a test can release jobs on demand.
        gated: bool,
    }

    impl FakeExecutor {
        fn new(gated: bool) -> Self {
            Self {
                log: StdMutex::new(Vec::new()),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                hold: Notify::new(),
                gated,
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().expect("锁").clone()
        }
    }

    #[async_trait::async_trait]
    impl BatchExecutor for FakeExecutor {
        async fn execute(&self, job: BatchJob) -> ExecOutcome {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.log
                .lock()
                .expect("锁")
                .push(format!("start:{}:{}", job.group_id, job.batch_id));
            if self.gated {
                self.hold.notified().await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            ExecOutcome::Done
        }
    }

    fn job(group: &str, batch: &str) -> BatchJob {
        BatchJob {
            group_id: group.into(),
            batch_id: batch.into(),
            event_ids: vec![1],
            privileged: false,
            chars: 10,
            images: 0,
            flooded: false,
            flood_background: None,
            retry: false,
        }
    }

    fn job_with_ids(group: &str, batch: &str, ids: &[i64], chars: usize) -> BatchJob {
        BatchJob {
            group_id: group.into(),
            batch_id: batch.into(),
            event_ids: ids.to_vec(),
            privileged: false,
            chars,
            images: 0,
            flooded: false,
            flood_background: None,
            retry: false,
        }
    }

    #[tokio::test]
    async fn same_group_runs_strictly_in_order() {
        let scheduler = Scheduler::new(4, BatchLimits::default());
        let exec = Arc::new(FakeExecutor::new(false));
        // Full batches (50 messages each) can never merge into one another, so all
        // five stay separate jobs; small batches folding together is covered by
        // `queued_batches_for_a_busy_lane_merge_into_one` instead.
        for i in 0..5 {
            let ids: Vec<i64> = (0..50).map(|n| i * 50 + n).collect();
            assert!(
                scheduler
                    .enqueue(
                        Arc::clone(&exec),
                        job_with_ids("100", &format!("b{i}"), &ids, 10)
                    )
                    .await
            );
        }
        for _ in 0..50 {
            if scheduler.queued("100").await == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let log = exec.log();
        let order: Vec<&str> = log.iter().map(|s| s.rsplit(':').next().unwrap()).collect();
        assert_eq!(order, vec!["b0", "b1", "b2", "b3", "b4"]);
    }

    #[tokio::test]
    async fn global_concurrency_is_bounded() {
        let scheduler = Scheduler::new(2, BatchLimits::default());
        let exec = Arc::new(FakeExecutor::new(false));
        for g in 0..6 {
            scheduler
                .enqueue(Arc::clone(&exec), job(&format!("{g}"), "b"))
                .await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            exec.peak.load(Ordering::SeqCst) <= 2,
            "全局并发不得超过 2，实际 {}",
            exec.peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn halted_lane_refuses_new_batches_until_resumed() {
        struct HaltOnce;
        #[async_trait::async_trait]
        impl BatchExecutor for HaltOnce {
            async fn execute(&self, _job: BatchJob) -> ExecOutcome {
                ExecOutcome::Halted
            }
        }
        let scheduler = Scheduler::new(2, BatchLimits::default());
        let exec = Arc::new(HaltOnce);
        assert!(scheduler.enqueue(Arc::clone(&exec), job("7", "b0")).await);
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        // New work for a halted lane is queued but explicitly not accepted.
        assert!(
            !scheduler.enqueue(Arc::clone(&exec), job("7", "b1")).await,
            "halted 车道不得接受新批次"
        );
        // A different group is unaffected.
        assert!(scheduler.enqueue(Arc::clone(&exec), job("8", "b0")).await);

        let snap = scheduler.snapshot().await;
        let lane7 = snap.iter().find(|l| l.group_id == "7").expect("车道 7");
        assert!(lane7.halted);

        // Resuming drops the unaccepted queue on purpose, and hands the
        // dropped jobs back so their rows can be released for re-claim.
        let dropped = scheduler.resume_lane("7").await;
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].batch_id, "b1");
        let snap = scheduler.snapshot().await;
        let lane7 = snap.iter().find(|l| l.group_id == "7").expect("车道 7");
        assert!(!lane7.halted);
        assert_eq!(lane7.queued, 0);
    }

    /// Executor that blocks every job until the test opens the gate, recording the
    /// exact jobs it ran so merge results can be asserted on what the agent would
    /// have been given.
    struct BlockingExecutor {
        started: StdMutex<Vec<BatchJob>>,
        open: tokio::sync::watch::Sender<bool>,
        gate: tokio::sync::watch::Receiver<bool>,
    }

    impl BlockingExecutor {
        fn new() -> Self {
            let (open, gate) = tokio::sync::watch::channel(false);
            Self {
                started: StdMutex::new(Vec::new()),
                open,
                gate,
            }
        }

        fn started(&self) -> Vec<BatchJob> {
            self.started.lock().expect("锁").clone()
        }

        fn release(&self) {
            let _ = self.open.send(true);
        }
    }

    #[async_trait::async_trait]
    impl BatchExecutor for BlockingExecutor {
        async fn execute(&self, job: BatchJob) -> ExecOutcome {
            self.started.lock().expect("锁").push(job);
            let mut gate = self.gate.clone();
            while !*gate.borrow() {
                if gate.changed().await.is_err() {
                    break;
                }
            }
            ExecOutcome::Done
        }
    }

    /// Poll until `cond` holds or the attempt budget runs out.
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..100 {
            if cond() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("等待条件超时");
    }

    #[tokio::test]
    async fn queued_batches_for_a_busy_lane_merge_into_one() {
        let scheduler = Scheduler::new(2, BatchLimits::default());
        let exec = Arc::new(BlockingExecutor::new());
        // First batch occupies the lane.
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b1", &[1], 10))
            .await;
        wait_until(|| exec.started().len() == 1).await;
        // Two more batches arrive while it runs: they must become one queued job,
        // ids unioned without loss or duplication, privileged OR-ed.
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b2", &[2, 3], 10))
            .await;
        let mut third = job_with_ids("100", "b3", &[3, 4], 10);
        third.privileged = true;
        scheduler.enqueue(Arc::clone(&exec), third).await;
        assert_eq!(
            scheduler.queued("100").await,
            1,
            "排队中的批次应合并为一个 job"
        );

        exec.release();
        wait_until(|| exec.started().len() == 2).await;
        let ran = exec.started();
        assert_eq!(ran[1].batch_id, "b2", "保留先到批次的 batch_id");
        assert_eq!(ran[1].event_ids, vec![2, 3, 4], "event_ids 不丢不重");
        assert!(ran[1].privileged, "privileged 取或");
    }

    #[tokio::test]
    async fn merging_never_crosses_groups() {
        let scheduler = Scheduler::new(2, BatchLimits::default());
        let exec = Arc::new(BlockingExecutor::new());
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b1", &[1], 10))
            .await;
        wait_until(|| exec.started().len() == 1).await;
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b2", &[2], 10))
            .await;
        // Another group has its own lane; its batch must not fold into group 100's.
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("200", "b9", &[9], 10))
            .await;
        wait_until(|| exec.started().len() == 2).await;
        let other = exec
            .started()
            .into_iter()
            .find(|j| j.group_id == "200")
            .expect("另一群的 job 应独立执行");
        assert_eq!(other.event_ids, vec![9]);
        assert_eq!(scheduler.queued("100").await, 1);
        exec.release();
    }

    #[tokio::test]
    async fn a_merge_that_would_break_the_caps_stays_a_separate_job() {
        // Message cap 3: the queued batch holds 2, the newcomer holds 2.
        let tight = BatchLimits {
            max_messages: 3,
            max_chars: 8_000,
            max_images: 4,
        };
        let scheduler = Scheduler::new(2, tight);
        let exec = Arc::new(BlockingExecutor::new());
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b1", &[1], 10))
            .await;
        wait_until(|| exec.started().len() == 1).await;
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b2", &[2, 3], 10))
            .await;
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b3", &[4, 5], 10))
            .await;
        assert_eq!(scheduler.queued("100").await, 2, "超消息上限不得合并");

        // Char cap: same shape, but the budget is characters instead of messages.
        let tight_chars = BatchLimits {
            max_messages: 50,
            max_chars: 100,
            max_images: 4,
        };
        let scheduler = Scheduler::new(2, tight_chars);
        let exec = Arc::new(BlockingExecutor::new());
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b1", &[1], 10))
            .await;
        wait_until(|| exec.started().len() == 1).await;
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b2", &[2], 60))
            .await;
        scheduler
            .enqueue(Arc::clone(&exec), job_with_ids("100", "b3", &[3], 60))
            .await;
        assert_eq!(scheduler.queued("100").await, 2, "超字数上限不得合并");
        exec.release();
    }
}
