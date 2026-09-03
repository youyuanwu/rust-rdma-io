//! Shared runtime capability checks for explicitly driven v2 futures.

#[cfg(panic = "unwind")]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(panic = "unwind")]
use std::sync::{Arc, Mutex, OnceLock};

use super::error::{Error, Result};

pub(crate) fn preflight_driver_runtime(driver_name: &str) -> Result<()> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(Error::InvalidConfig(format!(
            "{driver_name} must be polled inside an active Tokio runtime with time enabled"
        )));
    }
    #[cfg(panic = "unwind")]
    {
        match probe_runtime(|| tokio::time::sleep(std::time::Duration::ZERO)) {
            RuntimeProbe::Completed(sleep) => {
                drop(sleep);
                Ok(())
            }
            RuntimeProbe::Panicked => Err(Error::InvalidConfig(format!(
                "{driver_name} requires Tokio time to be enabled"
            ))),
        }
    }
    #[cfg(not(panic = "unwind"))]
    {
        // Tokio exposes no non-panicking query for its optional time driver.
        // Polling can progress without a timer until work arms a deadline.
        Ok(())
    }
}

#[cfg(panic = "unwind")]
pub(crate) enum RuntimeProbe<T> {
    Completed(T),
    Panicked,
}

#[cfg(panic = "unwind")]
pub(crate) fn probe_runtime<T>(probe: impl FnOnce() -> T) -> RuntimeProbe<T> {
    // Tokio exposes no capability query for optional I/O/time drivers. Serialize
    // the constructor probe and suppress only its current-thread panic; panics
    // from every other thread still reach the application's installed hook.
    static PROBE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _probe = PROBE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let thread = std::thread::current().id();
    type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;
    let previous: Arc<Mutex<Option<PanicHook>>> =
        Arc::new(Mutex::new(Some(std::panic::take_hook())));
    let fallback = Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() != thread {
            let hook = fallback.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(hook) = hook.as_ref() {
                hook(info);
            }
        }
    }));
    let result = catch_unwind(AssertUnwindSafe(probe));
    let previous = previous
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("runtime probe panic hook");
    std::panic::set_hook(previous);
    match result {
        Ok(value) => RuntimeProbe::Completed(value),
        Err(_) => RuntimeProbe::Panicked,
    }
}
