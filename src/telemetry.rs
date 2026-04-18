use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
/// Controls how much lifecycle tracing the crate emits.
pub enum TracingVerbosity {
    /// Disable crate-managed tracing events.
    Off,
    /// Emit lifecycle events such as queued, started, and completed.
    Lifecycle,
    /// Emit lifecycle events plus more detailed diagnostic events.
    Verbose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Global tracing configuration for this crate.
pub struct TracingConfig {
    /// Desired verbosity level for emitted tracing events.
    pub verbosity: TracingVerbosity,
}

impl TracingConfig {
    #[must_use]
    /// Override the tracing verbosity.
    pub fn verbosity(mut self, verbosity: TracingVerbosity) -> Self {
        self.verbosity = verbosity;
        self
    }
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            verbosity: TracingVerbosity::Lifecycle,
        }
    }
}

fn tracing_config_lock() -> &'static RwLock<TracingConfig> {
    static TRACING_CONFIG: OnceLock<RwLock<TracingConfig>> = OnceLock::new();
    TRACING_CONFIG.get_or_init(|| RwLock::new(TracingConfig::default()))
}

/// Replace the global tracing configuration.
pub fn set_tracing_config(config: TracingConfig) {
    *tracing_config_lock()
        .write()
        .expect("tracing config lock poisoned") = config;
}

#[must_use]
/// Read the current global tracing configuration.
pub fn tracing_config() -> TracingConfig {
    *tracing_config_lock()
        .read()
        .expect("tracing config lock poisoned")
}

#[must_use]
pub(crate) fn emit_lifecycle_traces() -> bool {
    tracing_config().verbosity >= TracingVerbosity::Lifecycle
}

#[must_use]
pub(crate) fn emit_verbose_traces() -> bool {
    tracing_config().verbosity >= TracingVerbosity::Verbose
}
