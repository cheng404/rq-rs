#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Async Sidekiq-compatible client and worker runtime for Rust.
//!
//! The crate provides two main entry points:
//! - [`Worker`] and [`WorkerOpts`] for strongly typed Rust workers.
//! - [`Processor`] for polling Redis and executing registered workers.
//!
//! Common enqueueing helpers:
//! - [`Worker::perform_async`] for immediate execution.
//! - [`Worker::perform_in`] for delayed execution.
//! - [`perform_async`] and [`perform_in`] for enqueueing by explicit class name.
//!
//! Additional APIs are available for periodic jobs ([`periodic`]), Redis namespacing
//! ([`with_custom_namespace`]), middleware ([`ServerMiddleware`]), and tracing
//! configuration ([`TracingConfig`]).

use async_trait::async_trait;
use middleware::Chain;
use rand::{Rng, RngCore};
use serde::{
    de::{self, Deserializer, Visitor},
    Deserialize, Serialize, Serializer,
};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

/// APIs for registering cron-style periodic jobs.
pub mod periodic;

mod middleware;
mod processor;
mod redis;
mod scheduled;
mod stats;
mod telemetry;

// Re-export
pub use crate::redis::{
    with_custom_namespace, RedisConnection, RedisConnectionManager, RedisError, RedisPool,
};
pub use ::redis as redis_rs;
pub use middleware::{ChainIter, ServerMiddleware};
#[cfg(feature = "opentelemetry")]
pub use processor::OpenTelemetryProcessMetrics;
pub use processor::{
    BalanceStrategy, ProcessStatsMode, Processor, ProcessorConfig, QueueConfig, WorkFetcher,
};
pub use scheduled::Scheduled;
pub use stats::{Counter, StatsPublisher};
pub use telemetry::{set_tracing_config, tracing_config, TracingConfig, TracingVerbosity};
use tracing::{debug, info, info_span, Instrument};

#[derive(thiserror::Error, Debug)]
/// Error type returned by this crate.
pub enum Error {
    /// A human-readable error message.
    #[error("{0}")]
    Message(String),

    /// JSON serialization or deserialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// Cron parsing or schedule calculation failed.
    #[error(transparent)]
    CronClock(#[from] cron_clock::error::Error),

    /// Redis pool acquisition failed.
    #[error(transparent)]
    BB8(#[from] bb8::RunError<redis::RedisError>),

    /// A chrono conversion overflow occurred.
    #[error(transparent)]
    ChronoRange(#[from] chrono::OutOfRangeError),

    /// A Redis command failed.
    #[error(transparent)]
    Redis(#[from] redis::RedisError),

    /// An arbitrary boxed error bubbled up from user code.
    #[error(transparent)]
    Any(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Result type used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[must_use]
/// Create default enqueue options for a job.
pub fn opts() -> EnqueueOpts {
    EnqueueOpts {
        queue: "default".into(),
        retry: RetryOpts::Yes,
        unique_for: None,
        retry_queue: None,
    }
}

/// Builder for enqueueing jobs by explicit class name.
pub struct EnqueueOpts {
    queue: String,
    retry: RetryOpts,
    unique_for: Option<std::time::Duration>,
    retry_queue: Option<String>,
}

impl EnqueueOpts {
    #[must_use]
    /// Override the target queue.
    pub fn queue<S: Into<String>>(self, queue: S) -> Self {
        Self {
            queue: queue.into(),
            ..self
        }
    }

    #[must_use]
    /// Override the retry policy for the job.
    pub fn retry<RO>(self, retry: RO) -> Self
    where
        RO: Into<RetryOpts>,
    {
        Self {
            retry: retry.into(),
            ..self
        }
    }

    #[must_use]
    /// Make a job unique for the provided duration.
    pub fn unique_for(self, unique_for: std::time::Duration) -> Self {
        Self {
            unique_for: Some(unique_for),
            ..self
        }
    }

    #[must_use]
    /// Route retries to a different queue.
    pub fn retry_queue(self, retry_queue: String) -> Self {
        Self {
            retry_queue: Some(retry_queue),
            ..self
        }
    }

    /// Create a raw Sidekiq job payload without enqueueing it.
    pub fn create_job(&self, class: String, args: impl serde::Serialize) -> Result<Job> {
        let args = serde_json::to_value(args)?;

        // Ensure args are always wrapped in an array.
        let args = if args.is_array() {
            args
        } else {
            JsonValue::Array(vec![args])
        };

        Ok(Job {
            queue: self.queue.clone(),
            class,
            jid: new_jid(),
            created_at: chrono::Utc::now().timestamp() as f64,
            enqueued_at: None,
            retry: self.retry.clone(),
            args,

            // Make default eventually...
            error_message: None,
            error_class: None,
            failed_at: None,
            retry_count: None,
            retried_at: None,

            // Meta for enqueueing
            retry_queue: self.retry_queue.clone(),
            unique_for: self.unique_for,
        })
    }

    /// Enqueue a job for immediate execution.
    pub async fn perform_async(
        self,
        redis: &RedisPool,
        class: String,
        args: impl serde::Serialize,
    ) -> Result<()> {
        let job = self.create_job(class, args)?;
        UnitOfWork::from_job(job).enqueue(redis).await?;
        Ok(())
    }

    /// Enqueue a job to run after the provided delay.
    pub async fn perform_in(
        &self,
        redis: &RedisPool,
        class: String,
        duration: std::time::Duration,
        args: impl serde::Serialize,
    ) -> Result<()> {
        let job = self.create_job(class, args)?;
        UnitOfWork::from_job(job).schedule(redis, duration).await?;
        Ok(())
    }
}

/// Helper function for enqueueing a worker into sidekiq.
/// This can be used to enqueue a job for a ruby sidekiq worker to process.
pub async fn perform_async(
    redis: &RedisPool,
    class: String,
    queue: String,
    args: impl serde::Serialize,
) -> Result<()> {
    opts().queue(queue).perform_async(redis, class, args).await
}

/// Helper function for enqueueing a worker into sidekiq.
/// This can be used to enqueue a delayed job for a ruby Sidekiq worker to process.
pub async fn perform_in(
    redis: &RedisPool,
    duration: std::time::Duration,
    class: String,
    queue: String,
    args: impl serde::Serialize,
) -> Result<()> {
    opts()
        .queue(queue)
        .perform_in(redis, class, duration, args)
        .await
}

fn new_jid() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Strongly typed enqueue options bound to a concrete [`Worker`] implementation.
pub struct WorkerOpts<Args, W: Worker<Args> + ?Sized> {
    queue: String,
    retry: RetryOpts,
    args: PhantomData<Args>,
    worker: PhantomData<W>,
    unique_for: Option<std::time::Duration>,
    retry_queue: Option<String>,
}

impl<Args, W> WorkerOpts<Args, W>
where
    W: Worker<Args>,
{
    #[must_use]
    /// Create a new set of worker enqueue options.
    pub fn new() -> Self {
        Self {
            queue: "default".into(),
            retry: RetryOpts::Yes,
            args: PhantomData,
            worker: PhantomData,
            unique_for: None,
            retry_queue: None,
        }
    }

    #[must_use]
    /// Override the retry policy for the worker invocation.
    pub fn retry<RO>(self, retry: RO) -> Self
    where
        RO: Into<RetryOpts>,
    {
        Self {
            retry: retry.into(),
            ..self
        }
    }

    #[must_use]
    /// Route retries for this worker invocation to a different queue.
    pub fn retry_queue<S: Into<String>>(self, retry_queue: S) -> Self {
        Self {
            retry_queue: Some(retry_queue.into()),
            ..self
        }
    }

    #[must_use]
    /// Override the queue for this worker invocation.
    pub fn queue<S: Into<String>>(self, queue: S) -> Self {
        Self {
            queue: queue.into(),
            ..self
        }
    }

    #[must_use]
    /// Make this worker invocation unique for the provided duration.
    pub fn unique_for(self, unique_for: std::time::Duration) -> Self {
        Self {
            unique_for: Some(unique_for),
            ..self
        }
    }

    #[allow(clippy::wrong_self_convention)]
    /// Convert worker-specific options into generic enqueue options.
    pub fn into_opts(&self) -> EnqueueOpts {
        self.into()
    }

    /// Enqueue the worker for immediate execution.
    pub async fn perform_async(
        &self,
        redis: &RedisPool,
        args: impl serde::Serialize + Send + 'static,
    ) -> Result<()> {
        self.into_opts()
            .perform_async(redis, W::class_name(), args)
            .await
    }

    /// Enqueue the worker to run after the provided delay.
    pub async fn perform_in(
        &self,
        redis: &RedisPool,
        duration: std::time::Duration,
        args: impl serde::Serialize + Send + 'static,
    ) -> Result<()> {
        self.into_opts()
            .perform_in(redis, W::class_name(), duration, args)
            .await
    }
}

impl<Args, W: Worker<Args>> From<&WorkerOpts<Args, W>> for EnqueueOpts {
    fn from(opts: &WorkerOpts<Args, W>) -> Self {
        Self {
            retry: opts.retry.clone(),
            queue: opts.queue.clone(),
            unique_for: opts.unique_for,
            retry_queue: opts.retry_queue.clone(),
        }
    }
}

impl<Args, W: Worker<Args>> Default for WorkerOpts<Args, W> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
/// Trait implemented by all Rust workers processed by this crate.
pub trait Worker<Args>: Send + Sync {
    /// Signal to WorkerRef to not attempt to modify the JsonValue args
    /// before calling the perform function. This is useful if the args
    /// are expected to be a `Vec<T>` that might be `len() == 1` or a
    /// single sized tuple `(T,)`.
    fn disable_argument_coercion(&self) -> bool {
        false
    }

    #[must_use]
    /// Return the default enqueue options for this worker type.
    fn opts() -> WorkerOpts<Args, Self>
    where
        Self: Sized,
    {
        WorkerOpts::new()
    }

    // TODO: Make configurable through opts and make opts accessible to the
    // retry middleware through a Box<dyn Worker>.
    /// Return the maximum number of retries allowed for failed jobs.
    fn max_retries(&self) -> usize {
        25
    }

    /// Derive a Sidekiq class name from the worker type.
    #[must_use]
    fn class_name() -> String
    where
        Self: Sized,
    {
        use convert_case::{Case, Casing};

        let type_name = std::any::type_name::<Self>();
        let name = type_name.split("::").last().unwrap_or(type_name);
        name.to_case(Case::UpperCamel)
    }

    /// Enqueue this worker for immediate execution using [`Worker::opts`].
    async fn perform_async(redis: &RedisPool, args: Args) -> Result<()>
    where
        Self: Sized,
        Args: Send + Sync + serde::Serialize + 'static,
    {
        Self::opts().perform_async(redis, args).await
    }

    /// Enqueue this worker to run after the provided delay using [`Worker::opts`].
    async fn perform_in(redis: &RedisPool, duration: std::time::Duration, args: Args) -> Result<()>
    where
        Self: Sized,
        Args: Send + Sync + serde::Serialize + 'static,
    {
        Self::opts().perform_in(redis, duration, args).await
    }

    /// Execute the job with strongly typed arguments.
    async fn perform(&self, args: Args) -> Result<()>;
}

// We can't store a Vec<Box<dyn Worker<Args>>>, because that will only work
// for a single arg type, but since any worker is JsonValue in and Result out,
// we can wrap that generic work in a callback that shares the same type.
// I'm sure this has a fancy name, but I don't know what it is.
#[derive(Clone)]
/// Type-erased wrapper used internally to store registered workers.
pub struct WorkerRef {
    #[allow(clippy::type_complexity)]
    work_fn: Arc<
        Box<dyn Fn(JsonValue) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>,
    >,
    max_retries: usize,
}

async fn invoke_worker<Args, W>(args: JsonValue, worker: Arc<W>) -> Result<()>
where
    Args: Send + Sync + 'static,
    W: Worker<Args> + 'static,
    for<'de> Args: Deserialize<'de>,
{
    let args = if worker.disable_argument_coercion() {
        args
    } else {
        // Ensure any caller expecting to receive `()` will always work.
        if std::any::TypeId::of::<Args>() == std::any::TypeId::of::<()>() {
            JsonValue::Null
        } else {
            // If the value contains a single item Vec then
            // you can probably be sure that this is a single value item.
            // Otherwise, the caller can impl a tuple type.
            match args {
                JsonValue::Array(mut arr) if arr.len() == 1 => {
                    arr.pop().expect("value change after size check")
                }
                _ => args,
            }
        }
    };

    let args: Args = serde_json::from_value(args)?;
    worker.perform(args).await
}

impl WorkerRef {
    pub(crate) fn wrap<Args, W>(worker: Arc<W>) -> Self
    where
        Args: Send + Sync + 'static,
        W: Worker<Args> + 'static,
        for<'de> Args: Deserialize<'de>,
    {
        Self {
            work_fn: Arc::new(Box::new({
                let worker = worker.clone();
                move |args: JsonValue| {
                    let worker = worker.clone();
                    Box::pin(async move { invoke_worker(args, worker).await })
                }
            })),
            max_retries: worker.max_retries(),
        }
    }

    #[must_use]
    /// Return the worker's configured maximum retry count.
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// Invoke the wrapped worker with JSON arguments.
    pub async fn call(&self, args: JsonValue) -> Result<()> {
        (Arc::clone(&self.work_fn))(args).await
    }
}

#[derive(Clone, Debug, PartialEq)]
/// Retry policy serialized into Sidekiq-compatible job payloads.
pub enum RetryOpts {
    /// Use the worker's default retry behavior.
    Yes,
    /// Disable retries for the job.
    Never,
    /// Retry the job at most the provided number of times.
    Max(usize),
}

impl Serialize for RetryOpts {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            RetryOpts::Yes => serializer.serialize_bool(true),
            RetryOpts::Never => serializer.serialize_bool(false),
            RetryOpts::Max(value) => serializer.serialize_u64(value as u64),
        }
    }
}

impl<'de> Deserialize<'de> for RetryOpts {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RetryOptsVisitor;

        impl Visitor<'_> for RetryOptsVisitor {
            type Value = RetryOpts;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a boolean, null, or a positive integer")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value {
                    Ok(RetryOpts::Yes)
                } else {
                    Ok(RetryOpts::Never)
                }
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(RetryOpts::Never)
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(RetryOpts::Max(value as usize))
            }
        }

        deserializer.deserialize_any(RetryOptsVisitor)
    }
}

impl From<bool> for RetryOpts {
    fn from(value: bool) -> Self {
        match value {
            true => RetryOpts::Yes,
            false => RetryOpts::Never,
        }
    }
}

impl From<usize> for RetryOpts {
    fn from(value: usize) -> Self {
        RetryOpts::Max(value)
    }
}

//
// {
//   "retry": true,
//   "queue": "yolo",
//   "class": "YoloWorker",
//   "args": [
//     {
//       "yolo": "hiiii"
//     }
//   ],
//   "jid": "f33f7063c6d7a4db0869289a",
//   "created_at": 1647119929.3788748,
//   "enqueued_at": 1647119929.378998
// }
//
#[derive(Serialize, Deserialize, Debug, Clone)]
/// Raw Sidekiq job payload stored in Redis.
pub struct Job {
    /// Queue name without the `queue:` Redis prefix.
    pub queue: String,
    /// Serialized job arguments.
    pub args: JsonValue,
    /// Retry policy for the job.
    pub retry: RetryOpts,
    /// Sidekiq worker class name.
    pub class: String,
    /// Unique job identifier.
    pub jid: String,
    /// Job creation timestamp as a Unix timestamp in seconds.
    pub created_at: f64,
    /// Time when the job was pushed to its queue.
    pub enqueued_at: Option<f64>,
    /// Time when the job first failed.
    pub failed_at: Option<f64>,
    /// Last recorded error message.
    pub error_message: Option<String>,
    /// Last recorded error class name.
    pub error_class: Option<String>,
    /// Number of retry attempts already performed.
    pub retry_count: Option<usize>,
    /// Time when the job was last retried.
    pub retried_at: Option<f64>,
    /// Optional queue name used for retries.
    pub retry_queue: Option<String>,

    #[serde(skip)]
    /// Duration used for de-duplication when unique jobs are enabled.
    pub unique_for: Option<std::time::Duration>,
}

#[derive(Debug)]
/// A fetched job together with the Redis queue key it belongs to.
pub struct UnitOfWork {
    /// Redis queue key, usually in the form `queue:<name>`.
    pub queue: String,
    /// Job payload to execute.
    pub job: Job,
}

impl UnitOfWork {
    #[must_use]
    /// Wrap a raw [`Job`] into a [`UnitOfWork`].
    pub fn from_job(job: Job) -> Self {
        Self {
            queue: format!("queue:{}", &job.queue),
            job,
        }
    }

    /// Deserialize a [`Job`] from JSON and wrap it in a [`UnitOfWork`].
    pub fn from_job_string(job_str: String) -> Result<Self> {
        let job: Job = serde_json::from_str(&job_str)?;
        Ok(Self::from_job(job))
    }

    /// Push the job into Redis immediately.
    pub async fn enqueue(&self, redis: &RedisPool) -> Result<()> {
        let mut redis = redis.get().await?;
        self.enqueue_direct(&mut redis).await
    }

    async fn enqueue_direct(&self, redis: &mut RedisConnection) -> Result<()> {
        let mut job = self.job.clone();
        let span = info_span!(
            "sidekiq.enqueue",
            jid = %job.jid,
            class = %job.class,
            queue = %job.queue,
            retry_count = job.retry_count.unwrap_or(0),
            worker_type = "async",
        );

        async {
            job.enqueued_at = Some(chrono::Utc::now().timestamp() as f64);

            if let Some(ref duration) = job.unique_for {
                // Check to see if this is unique for the given duration.
                // Even though SET k v NX EQ ttl isn't the best locking
                // mechanism, I think it's "good enough" to prove this out.
                let args_as_json_string: String = serde_json::to_string(&job.args)?;
                let args_hash = format!("{:x}", Sha256::digest(&args_as_json_string));
                let redis_key = format!(
                    "sidekiq:unique:{}:{}:{}",
                    &job.queue, &job.class, &args_hash
                );
                if let redis::RedisValue::Nil = redis
                    .set_nx_ex(redis_key, "", duration.as_secs() as usize)
                    .await?
                {
                    if telemetry::emit_verbose_traces() {
                        debug!(
                            status = "duplicate",
                            jid = %job.jid,
                            class = %job.class,
                            queue = %job.queue,
                            retry_count = job.retry_count.unwrap_or(0),
                            worker_type = "async",
                            "job.deduplicated"
                        );
                    }
                    return Ok(());
                }
            }

            redis.sadd("queues".to_string(), job.queue.clone()).await?;

            redis
                .lpush(self.queue.clone(), serde_json::to_string(&job)?)
                .await?;

            if telemetry::emit_lifecycle_traces() {
                info!(
                    status = "queued",
                    jid = %job.jid,
                    class = %job.class,
                    queue = %job.queue,
                    retry_count = job.retry_count.unwrap_or(0),
                    worker_type = "async",
                    "job.enqueued"
                );
            }

            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Schedule the job for retry using Sidekiq's retry sorted set.
    pub async fn reenqueue(&mut self, redis: &RedisPool) -> Result<()> {
        if let Some(retry_count) = self.job.retry_count {
            let retry_at = Self::retry_job_at(retry_count);

            redis
                .get()
                .await?
                .zadd(
                    "retry".to_string(),
                    serde_json::to_string(&self.job)?,
                    retry_at.timestamp(),
                )
                .await?;

            if telemetry::emit_lifecycle_traces() {
                info!(
                    status = "retry_scheduled",
                    jid = %self.job.jid,
                    class = %self.job.class,
                    queue = %self.job.queue,
                    retry_count,
                    worker_type = "retry",
                    enqueue_at = retry_at.timestamp(),
                    "job.retry_scheduled"
                );
            }
        }

        Ok(())
    }

    fn retry_job_at(count: usize) -> chrono::DateTime<chrono::Utc> {
        let seconds_to_delay =
            count.pow(4) + 15 + (rand::thread_rng().gen_range(0..30) * (count + 1));

        chrono::Utc::now() + chrono::Duration::seconds(seconds_to_delay as i64)
    }

    /// Schedule the job to run after the provided delay.
    pub async fn schedule(
        &mut self,
        redis: &RedisPool,
        duration: std::time::Duration,
    ) -> Result<()> {
        let span = info_span!(
            "sidekiq.schedule",
            jid = %self.job.jid,
            class = %self.job.class,
            queue = %self.job.queue,
            retry_count = self.job.retry_count.unwrap_or(0),
            worker_type = "scheduled",
        );

        async {
            let enqueue_at = chrono::Utc::now() + chrono::Duration::from_std(duration)?;

            redis
                .get()
                .await?
                .zadd(
                    "schedule".to_string(),
                    serde_json::to_string(&self.job)?,
                    enqueue_at.timestamp(),
                )
                .await?;

            if telemetry::emit_lifecycle_traces() {
                info!(
                    status = "scheduled",
                    jid = %self.job.jid,
                    class = %self.job.class,
                    queue = %self.job.queue,
                    retry_count = self.job.retry_count.unwrap_or(0),
                    worker_type = "scheduled",
                    enqueue_at = enqueue_at.timestamp(),
                    "job.scheduled"
                );
            }

            Ok(())
        }
        .instrument(span)
        .await
    }
}

#[cfg(test)]
mod test {
    use super::*;

    mod my {
        pub mod cool {
            pub mod workers {
                use super::super::super::super::*;

                #[allow(dead_code)]
                pub struct TestOpts;

                #[async_trait]
                impl Worker<()> for TestOpts {
                    fn opts() -> WorkerOpts<(), Self>
                    where
                        Self: Sized,
                    {
                        WorkerOpts::new()
                            // Test bool
                            .retry(false)
                            // Test usize
                            .retry(42)
                            // Test the new type
                            .retry(RetryOpts::Never)
                            .unique_for(std::time::Duration::from_secs(30))
                            .queue("yolo_quue")
                    }

                    async fn perform(&self, _args: ()) -> Result<()> {
                        Ok(())
                    }
                }

                pub struct X1Y2MyJob;

                #[async_trait]
                impl Worker<()> for X1Y2MyJob {
                    async fn perform(&self, _args: ()) -> Result<()> {
                        Ok(())
                    }
                }

                pub struct TestModuleWorker;

                #[async_trait]
                impl Worker<()> for TestModuleWorker {
                    async fn perform(&self, _args: ()) -> Result<()> {
                        Ok(())
                    }
                }

                pub struct TestCustomClassNameWorker;

                #[async_trait]
                impl Worker<()> for TestCustomClassNameWorker {
                    async fn perform(&self, _args: ()) -> Result<()> {
                        Ok(())
                    }

                    fn class_name() -> String
                    where
                        Self: Sized,
                    {
                        "My::Cool::Workers::TestCustomClassNameWorker".to_string()
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn ignores_modules_in_ruby_worker_name() {
        assert_eq!(
            my::cool::workers::TestModuleWorker::class_name(),
            "TestModuleWorker".to_string()
        );
    }

    #[tokio::test]
    async fn does_not_reformat_valid_ruby_class_names() {
        assert_eq!(
            my::cool::workers::X1Y2MyJob::class_name(),
            "X1Y2MyJob".to_string()
        );
    }

    #[tokio::test]
    async fn supports_custom_class_name_for_workers() {
        assert_eq!(
            my::cool::workers::TestCustomClassNameWorker::class_name(),
            "My::Cool::Workers::TestCustomClassNameWorker".to_string()
        );
    }

    #[derive(Clone, Deserialize, Serialize, Debug)]
    struct TestArg {
        name: String,
        age: i32,
    }

    struct TestGenericWorker;
    #[async_trait]
    impl Worker<TestArg> for TestGenericWorker {
        async fn perform(&self, _args: TestArg) -> Result<()> {
            Ok(())
        }
    }

    struct TestMultiArgWorker;
    #[async_trait]
    impl Worker<(TestArg, TestArg)> for TestMultiArgWorker {
        async fn perform(&self, _args: (TestArg, TestArg)) -> Result<()> {
            Ok(())
        }
    }

    struct TestTupleArgWorker;
    #[async_trait]
    impl Worker<(TestArg,)> for TestTupleArgWorker {
        fn disable_argument_coercion(&self) -> bool {
            true
        }
        async fn perform(&self, _args: (TestArg,)) -> Result<()> {
            Ok(())
        }
    }

    struct TestVecArgWorker;
    #[async_trait]
    impl Worker<Vec<TestArg>> for TestVecArgWorker {
        fn disable_argument_coercion(&self) -> bool {
            true
        }
        async fn perform(&self, _args: Vec<TestArg>) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn can_have_a_vec_with_one_or_more_items() {
        // One item
        let worker = Arc::new(TestVecArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value(vec![TestArg {
            name: "test A".into(),
            age: 1337,
        }])
        .unwrap();
        wrap.call(arg).await.unwrap();

        // Multiple items
        let worker = Arc::new(TestVecArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value(vec![
            TestArg {
                name: "test A".into(),
                age: 1337,
            },
            TestArg {
                name: "test A".into(),
                age: 1337,
            },
        ])
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn can_have_multiple_arguments() {
        let worker = Arc::new(TestMultiArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value((
            TestArg {
                name: "test A".into(),
                age: 1337,
            },
            TestArg {
                name: "test B".into(),
                age: 1336,
            },
        ))
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn can_have_a_single_tuple_argument() {
        let worker = Arc::new(TestTupleArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value((TestArg {
            name: "test".into(),
            age: 1337,
        },))
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn can_have_a_single_argument() {
        let worker = Arc::new(TestGenericWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value(TestArg {
            name: "test".into(),
            age: 1337,
        })
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn processor_config_has_workers_by_default() {
        let cfg = ProcessorConfig::default();

        assert!(
            cfg.num_workers > 0,
            "num_workers should be greater than 0 (using num cpu)"
        );

        let cfg = cfg.num_workers(1000);

        assert_eq!(cfg.num_workers, 1000);
    }
}
