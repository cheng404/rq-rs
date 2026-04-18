use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum TracingVerbosity {
    Off,
    Lifecycle,
    Verbose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TracingConfig {
    pub verbosity: TracingVerbosity,
}

impl TracingConfig {
    #[must_use]
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

pub fn set_tracing_config(config: TracingConfig) {
    *tracing_config_lock()
        .write()
        .expect("tracing config lock poisoned") = config;
}

#[must_use]
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
