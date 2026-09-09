//! Ordered subprocess policy for optional session start/end hooks.
//!
//! IronRDP owns authentication and connection establishment; this module only
//! receives balanced start/end notifications. Commands run on one queue with a
//! bounded ordering deadline. Overdue commands are left running and reaped if
//! they exit before the server does.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) struct SessionHooks {
    session_active: bool,
    jobs: Option<mpsc::Sender<HookJob>>,
    shutting_down: Arc<AtomicBool>,
    runner: Option<std::thread::JoinHandle<()>>,
}

enum HookJob {
    SessionStart,
    SessionEnd,
}

const SESSION_HOOK_DEADLINE: Duration = Duration::from_secs(10);

const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(50);

const HOOK_SESSION_START: &str = "session_start";
const HOOK_SESSION_END: &str = "session_end";

pub(super) fn session_hooks_from_config(
    on_session_start: Option<String>,
    on_session_end: Option<String>,
    instance: Option<String>,
) -> Option<SessionHooks> {
    let on_session_start = on_session_start.filter(|command| !command.trim().is_empty());
    let on_session_end = on_session_end.filter(|command| !command.trim().is_empty());
    if on_session_start.is_none() && on_session_end.is_none() {
        return None;
    }
    Some(SessionHooks::spawn(
        on_session_start,
        on_session_end,
        SESSION_HOOK_DEADLINE,
        instance,
    ))
}

impl SessionHooks {
    fn spawn(
        on_session_start: Option<String>,
        on_session_end: Option<String>,
        deadline: Duration,
        instance: Option<String>,
    ) -> Self {
        let (jobs, queue) = mpsc::channel();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let queue_shutdown = Arc::clone(&shutting_down);
        let runner = std::thread::Builder::new()
            .name("session-hooks".into())
            .spawn(move || {
                run_hook_queue(
                    &queue,
                    &queue_shutdown,
                    on_session_start,
                    on_session_end,
                    deadline,
                    instance,
                );
            });
        let runner = match runner {
            Ok(runner) => Some(runner),
            Err(error) => {
                tracing::warn!(%error, "Failed to start the session hook thread");
                None
            }
        };
        Self {
            session_active: false,
            jobs: runner.is_some().then_some(jobs),
            shutting_down,
            runner,
        }
    }

    fn send(&self, job: HookJob) {
        if let Some(jobs) = &self.jobs {
            if jobs.send(job).is_err() {
                tracing::warn!("Session hook thread is gone; command not queued");
            }
        }
    }

    pub(super) fn session_started(&mut self) {
        if self.session_active {
            return;
        }
        self.session_active = true;
        tracing::debug!("Session established");
        self.send(HookJob::SessionStart);
    }

    pub(super) fn session_ended(&mut self) {
        if !self.session_active {
            return;
        }
        self.session_active = false;
        tracing::debug!("Session ended");
        self.send(HookJob::SessionEnd);
    }
}

struct RunningHook {
    hook: &'static str,
    child: Child,
    started: Instant,
}

fn run_hook_queue(
    jobs: &mpsc::Receiver<HookJob>,
    shutting_down: &AtomicBool,
    on_session_start: Option<String>,
    on_session_end: Option<String>,
    deadline: Duration,
    instance: Option<String>,
) {
    let mut running: Option<RunningHook> = None;
    let mut stragglers: Vec<RunningHook> = Vec::new();
    // Once the handler is dropped the whole remaining queue shares one
    // budget, so a stuck command cannot multiply the stop time by the number
    // of jobs behind it.
    let mut drain_until: Option<Instant> = None;

    while let Ok(job) = jobs.recv() {
        reap_finished(&mut stragglers);
        if drain_until.is_none() && shutting_down.load(Ordering::Acquire) {
            drain_until = Some(Instant::now() + deadline);
        }
        let command = match job {
            HookJob::SessionStart => on_session_start.as_deref().map(|c| (HOOK_SESSION_START, c)),
            HookJob::SessionEnd => on_session_end.as_deref().map(|c| (HOOK_SESSION_END, c)),
        };
        let Some((hook, command)) = command else {
            // Keep the current command ordered against a later configured
            // hook, including the next start after a fast reconnect.
            continue;
        };

        finish_running(&mut running, &mut stragglers, deadline, drain_until);
        running = spawn_session_hook(hook, command, instance.clone());
    }

    // Shutdown: only the end command is worth waiting for — nothing is
    // ordered after a start command once the server is going away.
    match running.take() {
        Some(hook) if hook.hook == HOOK_SESSION_END => {
            let mut hook = Some(hook);
            finish_running(&mut hook, &mut stragglers, deadline, drain_until);
        }
        Some(hook) => stragglers.push(hook),
        None => {}
    }
    let reap_until = Instant::now() + HOOK_POLL_INTERVAL * 4;
    while !stragglers.is_empty() {
        reap_finished(&mut stragglers);
        if stragglers.is_empty() || Instant::now() >= reap_until {
            break;
        }
        std::thread::sleep(HOOK_POLL_INTERVAL);
    }
}

fn finish_running(
    running: &mut Option<RunningHook>,
    stragglers: &mut Vec<RunningHook>,
    deadline: Duration,
    drain_until: Option<Instant>,
) {
    let Some(mut hook) = running.take() else {
        return;
    };
    let wait_until = match drain_until {
        Some(drain) => (hook.started + deadline).min(drain),
        None => hook.started + deadline,
    };
    loop {
        match hook.child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    tracing::debug!(hook = hook.hook, "Session hook finished");
                } else {
                    tracing::warn!(hook = hook.hook, %status, "Session hook exited with failure");
                }
                return;
            }
            Ok(None) if Instant::now() < wait_until => {
                std::thread::sleep(
                    HOOK_POLL_INTERVAL.min(wait_until.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                tracing::warn!(
                    hook = hook.hook,
                    deadline_secs = deadline.as_secs(),
                    "Session hook still running past the ordering deadline; continuing alongside it"
                );
                stragglers.push(hook);
                return;
            }
            Err(error) => {
                tracing::warn!(hook = hook.hook, %error, "Failed to wait for session hook");
                return;
            }
        }
    }
}

fn reap_finished(stragglers: &mut Vec<RunningHook>) {
    stragglers.retain_mut(|hook| match hook.child.try_wait() {
        Ok(Some(status)) => {
            if !status.success() {
                tracing::warn!(hook = hook.hook, %status, "Session hook exited with failure");
            }
            false
        }
        Ok(None) => true,
        Err(error) => {
            tracing::warn!(hook = hook.hook, %error, "Failed to wait for session hook");
            false
        }
    });
}

fn hook_command(command: &str, instance: Option<String>) -> Command {
    let mut child = Command::new("/bin/sh");
    child.arg("-c").arg(command).stdin(Stdio::null());
    if let Some(instance) = instance {
        child.env("HYPRLAND_INSTANCE_SIGNATURE", instance);
    }
    child
}

fn spawn_session_hook(
    hook: &'static str,
    command: &str,
    instance: Option<String>,
) -> Option<RunningHook> {
    tracing::info!(hook, "Running session hook");
    // Deliberately not logging the command text: a hook string may embed
    // tokens, passwords or other secrets that must not reach the log.
    match hook_command(command, instance).spawn() {
        Ok(child) => Some(RunningHook {
            hook,
            child,
            started: Instant::now(),
        }),
        Err(error) => {
            tracing::warn!(hook, %error, "Failed to spawn /bin/sh for the session hook");
            None
        }
    }
}

impl Drop for SessionHooks {
    fn drop(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        if self.session_active {
            self.session_active = false;
            self.send(HookJob::SessionEnd);
        }
        drop(self.jobs.take());
        if let Some(runner) = self.runner.take() {
            if runner.join().is_err() {
                tracing::warn!("Session hook thread panicked");
            }
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use std::path::{Path, PathBuf};

    pub(in crate::server) fn test_hooks(
        log: &Path,
        connect_command: Option<String>,
        disconnect: bool,
    ) -> SessionHooks {
        SessionHooks::spawn(
            connect_command,
            disconnect.then(|| echo_to_log(log, "end")),
            Duration::from_secs(10),
            None,
        )
    }

    pub(in crate::server) fn echo_to_log(log: &Path, word: &str) -> String {
        format!("echo {word} >> '{}'", log.display())
    }

    pub(in crate::server) fn echo_start(log: &Path, prefix: &str) -> Option<String> {
        Some(format!("{prefix}{}", echo_to_log(log, "start")))
    }

    pub(in crate::server) fn hook_log_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("hypr-rdp-hook-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Generous ceiling for positive waits: loaded CI runners stall threads
    /// for seconds; a matching run still returns at the first poll that sees
    /// the expected content.
    pub(in crate::server) const LOG_CEILING: Duration = Duration::from_secs(30);

    pub(in crate::server) fn wait_for_nonempty_log(path: &Path) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if !content.trim().is_empty() || std::time::Instant::now() > deadline {
                return content;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub(in crate::server) fn wait_for_log(
        path: &Path,
        expected: &str,
        ceiling: Duration,
    ) -> String {
        let deadline = std::time::Instant::now() + ceiling;
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if content == expected || std::time::Instant::now() > deadline {
                return content;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn env_of(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn a_hook_is_given_the_resolved_instance() {
        let command = hook_command("true", Some("resolved-sig".into()));

        assert_eq!(
            env_of(&command),
            vec![(
                "HYPRLAND_INSTANCE_SIGNATURE".to_string(),
                Some("resolved-sig".to_string())
            )]
        );
    }

    #[test]
    fn missing_hook_commands_disable_connection_handler_wiring() {
        assert!(session_hooks_from_config(None, None, None).is_none());
        assert!(session_hooks_from_config(Some("true".into()), None, None).is_some());
        assert!(session_hooks_from_config(None, Some("true".into()), None).is_some());
    }

    #[test]
    fn probe_disconnect_without_session_fires_no_hooks() {
        let log = hook_log_path("probe");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_ended();
        drop(hooks);

        // Negative watch: a regression that fires the start hook writes the
        // file a few milliseconds after the drop-join returns.
        let watch_until = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < watch_until {
            assert_eq!(std::fs::read_to_string(&log).unwrap_or_default(), "");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn session_lifecycle_runs_start_then_end_in_order() {
        let log = hook_log_path("order");
        let mut hooks = test_hooks(&log, echo_start(&log, "sleep 0.3; "), true);

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn hook_calls_do_not_block_the_connection_handler() {
        let log = hook_log_path("nonblocking");
        // Small deadline so the implicit drop-join at the end stays cheap.
        let mut hooks = SessionHooks::spawn(
            Some("exec sleep 30 >/dev/null 2>&1".into()),
            Some(echo_to_log(&log, "end")),
            Duration::from_millis(300),
            None,
        );

        let start = std::time::Instant::now();
        hooks.session_started();
        hooks.session_ended();

        // The handler only queues jobs; the ordered wait happens on the hook
        // thread. A blocking implementation would sit out the full sleep;
        // the bound only needs to stay far under that.
        assert!(start.elapsed() < Duration::from_secs(2));

        drop(hooks);
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn deadline_releases_the_end_hook_past_a_stuck_start_command() {
        let log = hook_log_path("deadline");
        // The sleeper outlives the test process by up to 30s — acceptable on
        // ephemeral runners, and the gap to the 10s ceiling below is what
        // makes an ignored deadline a deterministic failure.
        let mut hooks = SessionHooks::spawn(
            Some("exec sleep 30 >/dev/null 2>&1".into()),
            Some(echo_to_log(&log, "end")),
            Duration::from_millis(100),
            None,
        );

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(
            wait_for_log(&log, "end\n", Duration::from_secs(10)),
            "end\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn start_only_configuration_skips_the_ordered_wait() {
        let log = hook_log_path("connect-only");
        let mut hooks = test_hooks(&log, Some("exec sleep 30 >/dev/null 2>&1".into()), false);

        hooks.session_started();
        hooks.session_ended();

        let start = std::time::Instant::now();
        drop(hooks); // joins the hook thread
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "no end command configured, so nothing may wait on the start command"
        );
    }

    #[test]
    fn fast_reconnect_does_not_overtake_the_previous_start_command() {
        let log = hook_log_path("no-cmd-hold");
        // A commandless end boundary must not release the previous start.
        let command = format!(
            "echo start >> '{}'; sleep 0.4; echo done >> '{}'",
            log.display(),
            log.display()
        );
        let mut hooks = SessionHooks::spawn(Some(command), None, Duration::from_secs(10), None);

        hooks.session_started();
        hooks.session_ended(); // no end command: must hold the running start
        hooks.session_started(); // reconnect
        drop(hooks);

        assert_eq!(
            wait_for_log(&log, "start\ndone\nstart\ndone\n", Duration::from_secs(5),),
            "start\ndone\nstart\ndone\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn sequential_sessions_reuse_the_handler_in_order() {
        let log = hook_log_path("cycles");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        let mut expected = String::new();
        for _ in 0..2 {
            hooks.session_started();
            hooks.session_ended();
            expected.push_str("start\nend\n");
            assert_eq!(wait_for_log(&log, &expected, LOG_CEILING), expected);
        }
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn config_wiring_passes_the_commands_through_in_order() {
        let log = hook_log_path("wiring");
        let mut hooks = session_hooks_from_config(
            Some(echo_to_log(&log, "start")),
            Some(echo_to_log(&log, "end")),
            None,
        )
        .expect("both commands configured");

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn end_only_configuration_runs_the_end_hook() {
        let log = hook_log_path("disconnect-only");
        let mut hooks = test_hooks(&log, None, true);

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(wait_for_log(&log, "end\n", LOG_CEILING), "end\n");
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn failed_start_command_does_not_delay_the_end_hook() {
        let log = hook_log_path("failed-connect");
        let mut hooks = test_hooks(&log, Some("false".into()), true);

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(wait_for_log(&log, "end\n", Duration::from_secs(5)), "end\n");
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn shutdown_drain_is_bounded_by_the_deadline() {
        let log = hook_log_path("drain-bound");
        let mut hooks = SessionHooks::spawn(
            Some(echo_to_log(&log, "start")),
            Some("exec sleep 30 >/dev/null 2>&1".into()),
            Duration::from_millis(200),
            None,
        );

        hooks.session_started();

        let start = std::time::Instant::now();
        drop(hooks); // queues SessionEnd, drains with a bounded wait
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "an end command slower than the deadline must not stall shutdown"
        );
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn dropping_the_handler_mid_session_still_runs_the_end_hook() {
        let log = hook_log_path("drop");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_started();
        drop(hooks); // service stop: the do/undo pair must still complete

        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn finished_hook_processes_are_reaped() {
        let log = hook_log_path("reap");
        // The command reports its own pid, so the check cannot be confused by
        // the children of the tests running in parallel.
        let mut hooks = SessionHooks::spawn(
            Some(format!("echo $$ >> '{}'", log.display())),
            None,
            Duration::from_secs(10),
            None,
        );

        hooks.session_started();
        drop(hooks);

        let pid = wait_for_nonempty_log(&log);
        let pid = pid.trim();
        // A queue that spawns without reaping leaves the command in the
        // process table for the lifetime of the service.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // "<pid> (<comm>) <state> ..." — comm itself may contain spaces.
            let zombie = stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
            if !zombie {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "hook process {pid} was never reaped"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn a_completed_session_does_not_fire_a_second_end_hook_on_drop() {
        let log = hook_log_path("no-double-end");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_started();
        hooks.session_ended();
        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );

        drop(hooks);
        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn repeated_session_starts_queue_one_command() {
        let log = hook_log_path("double-start");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_started();
        hooks.session_started();
        hooks.session_ended();
        drop(hooks);

        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn dropping_the_handler_is_bounded_even_with_a_job_in_flight() {
        let log = hook_log_path("drop-bound-deadlock");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);
        hooks.session_started();

        // Drop on a helper thread: closing the job channel after joining the
        // queue thread deadlocks, which would hang the whole suite instead of
        // failing this test.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(hooks);
            let _ = done.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(30)).is_ok(),
            "drop must close the job channel before joining the queue thread"
        );
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn hook_commands_cannot_read_the_server_stdin() {
        let log = hook_log_path("stdin");
        let mut hooks = SessionHooks::spawn(
            Some(format!("readlink /proc/self/fd/0 >> '{}'", log.display())),
            None,
            Duration::from_secs(10),
            None,
        );

        hooks.session_started();
        drop(hooks);

        assert_eq!(
            wait_for_log(&log, "/dev/null\n", LOG_CEILING),
            "/dev/null\n",
            "a hook must not inherit the server's stdin"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn the_session_hook_deadline_stays_short_enough_for_a_service_stop() {
        // The constant bounds the ordering wait and the shutdown drain.
        // systemd's default TimeoutStopSec is 90s: anything near it turns a
        // stuck hook into a SIGKILL, and zero removes the ordering guarantee.
        assert!(SESSION_HOOK_DEADLINE >= Duration::from_secs(1));
        assert!(SESSION_HOOK_DEADLINE <= Duration::from_secs(30));
    }
}
