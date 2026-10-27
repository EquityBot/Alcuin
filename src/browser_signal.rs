//! Reference registerTerminationHooks in the process that owns Chrome: Ctrl-C on every
//! platform, SIGTERM and SIGQUIT on Unix.
use crate::browser_chrome::SignalTarget;
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio_util::sync::CancellationToken;

/// Reference handleSignal: whether Chrome stays running, and the log lines in order.
pub(crate) fn plan(
    signal: &str,
    keep_browser: bool,
    in_flight: bool,
    preserve_shared: bool,
    force_cleanup: bool,
) -> (bool, Vec<String>) {
    let leave_running = (keep_browser || in_flight || preserve_shared) && !force_cleanup;
    let mut lines = vec![if leave_running {
        format!(
            "Received {signal}; leaving Chrome running{}",
            if in_flight {
                " (assistant response pending)"
            } else {
                ""
            }
        )
    } else if force_cleanup && (keep_browser || in_flight) {
        format!(
            "Received {signal}; terminating Chrome and removing the copied profile (copy-profile is not retained)"
        )
    } else {
        format!("Received {signal}; terminating Chrome process")
    }];
    // Runtime identity is already durable: every runtime_patch is a synchronous session write.
    if leave_running && in_flight {
        lines.push(
            "Session still in flight; reattach with \"industrious session <slug>\" to continue."
                .into(),
        );
    }
    (leave_running, lines)
}

/// Reference exit code after SIGINT; SIGTERM and SIGQUIT exit 1.
pub(crate) const INTERRUPT_EXIT_CODE: i32 = 130;
static INSTALLED: AtomicBool = AtomicBool::new(false);
static HANDLING: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(0);
static HOOKS: Mutex<Vec<(u64, Hook)>> = Mutex::new(Vec::new());

pub(crate) struct Hook {
    keep_browser: bool,
    /// Reference preserveSharedChromeOnSignal: the shared manual-login Chrome.
    preserve_shared: bool,
    /// Reference forceProfileCleanup: a copied profile is never retained.
    force_cleanup: bool,
    target: SignalTarget,
    log: Box<dyn Fn(&str) + Send>,
    in_flight: Arc<AtomicBool>,
}
impl Hook {
    pub(crate) fn new(
        keep_browser: bool,
        preserve_shared: bool,
        force_cleanup: bool,
        target: SignalTarget,
        log: Box<dyn Fn(&str) + Send>,
    ) -> Self {
        Self {
            keep_browser,
            preserve_shared,
            force_cleanup,
            target,
            log,
            in_flight: Arc::new(AtomicBool::new(true)),
        }
    }
    fn handle(&self, signal: &str) {
        let (leave_running, lines) = plan(
            signal,
            self.keep_browser,
            self.in_flight.load(Ordering::SeqCst),
            self.preserve_shared,
            self.force_cleanup,
        );
        (self.log)(&lines[0]);
        if !leave_running {
            self.target.terminate();
        }
        for line in &lines[1..] {
            (self.log)(line);
        }
    }
}
/// Removes its hook when dropped (reference removeTerminationHooks).
pub(crate) struct Registration {
    id: u64,
    in_flight: Arc<AtomicBool>,
}
impl Registration {
    /// Reference runStatus "complete": the answer was captured.
    pub(crate) fn complete(&self) {
        self.in_flight.store(false, Ordering::SeqCst);
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}
/// A hook acts only in a process that installed the handlers.
pub(crate) fn register(hook: Hook) -> Registration {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    let in_flight = hook.in_flight.clone();
    if INSTALLED.load(Ordering::SeqCst) {
        HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, hook));
    }
    Registration { id, in_flight }
}
/// Marks a process that may own Chrome; on Unix it also installs SIGTERM and SIGQUIT
/// handlers. Services keep their own shutdown signals and never install these.
pub(crate) fn install() {
    INSTALLED.store(true, Ordering::SeqCst);
    #[cfg(unix)]
    {
        static TERMINATION: std::sync::Once = std::sync::Once::new();
        TERMINATION.call_once(termination_handlers);
    }
}
#[cfg(unix)]
fn termination_handlers() {
    use tokio::signal::unix::{SignalKind, signal};
    for (kind, name, number) in [
        (SignalKind::terminate(), "SIGTERM", libc::SIGTERM),
        (SignalKind::quit(), "SIGQUIT", libc::SIGQUIT),
    ] {
        match signal(kind) {
            Ok(mut stream) => {
                tokio::spawn(async move {
                    if stream.recv().await.is_some() && !run_hooks(name, 1, || {}) {
                        default_disposition(number);
                    }
                });
            }
            Err(error) => eprintln!("{name} handler was not installed: {error}"),
        }
    }
}
/// Runs every registered hook and exits; false when no browser run owns Chrome.
fn run_hooks(signal: &str, code: i32, before: impl FnOnce()) -> bool {
    let hooks = {
        let mut registered = HOOKS.lock().unwrap_or_else(PoisonError::into_inner);
        // Reference `handling`: a later signal never interrupts the first one's cleanup.
        if HANDLING.load(Ordering::SeqCst) {
            return true;
        }
        if registered.is_empty() {
            return false;
        }
        HANDLING.store(true, Ordering::SeqCst);
        std::mem::take(&mut *registered)
    };
    before();
    for (_, hook) in &hooks {
        hook.handle(signal);
    }
    std::process::exit(code);
}
/// CLI Ctrl-C (reference top-level handler, then the browser hooks). While a browser run owns
/// Chrome, print "Cancelled.", run its hooks and exit 130 without cancelling or settling the
/// run, so its session stays reattachable. Otherwise cancel the run as before.
pub(crate) fn cancel_on_ctrl_c(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok()
            && !run_hooks("SIGINT", INTERRUPT_EXIT_CODE, || println!("\nCancelled."))
        {
            cancel.cancel();
        }
    })
}
/// MCP has no top-level handler: without a browser hook, Ctrl-C keeps its default action.
pub(crate) fn exit_on_ctrl_c() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() && !run_hooks("SIGINT", INTERRUPT_EXIT_CODE, || {})
        {
            #[cfg(unix)]
            default_disposition(libc::SIGINT);
            // The default console handler exits with STATUS_CONTROL_C_EXIT.
            #[cfg(windows)]
            std::process::exit(0xC000013A_u32 as i32);
        }
    });
}
#[cfg(unix)]
fn default_disposition(number: i32) {
    // SAFETY: restoring SIG_DFL and re-raising only reproduces the uninstalled behaviour.
    unsafe {
        libc::signal(number, libc::SIG_DFL);
        libc::raise(number);
    }
    std::process::exit(128 + number);
}

#[cfg(test)]
#[path = "browser_signal_tests.rs"]
mod tests;
