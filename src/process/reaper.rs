//! Reaping orphaned descendants: the bot is the container's PID 1, so
//! every grandchild whose parent dies is reparented to it — the quota
//! scrape's agy subtree, a dropped session's language server, canary
//! children. Nothing else waits on them, so each one that exits stays a
//! zombie task in the pids cgroup. Observed 2026-10-31: 127 zombies
//! against pids.max 512 (pids.current 484), arriving in pairs on the
//! quota scrape's 600s cadence; the health probe eventually failed to
//! spawn ("Resource temporarily unavailable") and docker marked the
//! container unhealthy.
//!
//! The sweep runs on a 30s interval rather than on SIGCHLD on purpose:
//! children with a live owner (tokio's mihomo child, portable-pty
//! session children) are reaped by that owner within milliseconds of
//! exiting, so the interval sweep only ever collects true orphans and
//! never steals an exit status a supervisor is waiting for.

use std::time::Duration;

#[cfg(unix)]
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
#[cfg(unix)]
use nix::unistd::Pid;

/// One sweep: reap every exited child nobody is waiting on. Returns how
/// many were collected.
#[cfg(unix)]
pub fn reap_orphaned_children() -> usize {
    let mut reaped = 0;
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(_) => reaped += 1,
            Err(_) => break,
        }
    }
    reaped
}

/// Off unix there is no orphan-reaping duty: the bot is PID 1 only
/// inside its Linux container; elsewhere every child keeps a live owner.
#[cfg(not(unix))]
pub fn reap_orphaned_children() -> usize {
    0
}

/// Spawn the periodic sweeper for the life of the process.
pub fn spawn_reaper() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            let n = reap_orphaned_children();
            if n > 0 {
                tracing::debug!(reaped = n, "回收孤儿进程僵尸");
            }
        }
    });
}
