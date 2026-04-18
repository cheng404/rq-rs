Sidekiq.rs (aka `rusty-sidekiq`)
================================

[![crates.io](https://img.shields.io/crates/v/rusty-sidekiq.svg)](https://crates.io/crates/rusty-sidekiq/)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE.md)
[![Documentation](https://docs.rs/rusty-sidekiq/badge.svg)](https://docs.rs/rusty-sidekiq/)

This is a reimplementation of sidekiq in rust. It is compatible with sidekiq.rb for both submitting and processing jobs.
Sidekiq.rb is obviously much more mature than this repo, but I hope you enjoy using it. This library is built using tokio
so it is async by default.

Current dependency notes:

- The library uses `redis` `1.1` and configures async multiplexed connections without a response timeout so blocking queue fetches such as `BRPOP` continue to work as expected.
- `Cargo.toml` uses semver-compatible version requirements rather than pinning patch versions, which is the recommended style for reusable Rust libraries.

## Installation

Add the library to your project:

```toml
[dependencies]
rusty-sidekiq = "0.13"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

To export processor process stats with OpenTelemetry instead of Sidekiq/Web-compatible Redis
hashes, enable the optional feature:

```toml
[dependencies]
rusty-sidekiq = { version = "0.13", features = ["opentelemetry"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Tracing

The crate emits structured `tracing` events for enqueueing, scheduling, retries, periodic jobs,
and worker execution. Applications remain responsible for installing their own subscriber:

```rust
use sidekiq::{set_tracing_config, TracingConfig, TracingVerbosity};
use tracing_subscriber::{fmt, EnvFilter};

set_tracing_config(TracingConfig::default().verbosity(TracingVerbosity::Verbose));

fmt()
    .with_env_filter(EnvFilter::from_default_env())
    .json()
    .init();
```

`TracingVerbosity::Lifecycle` is the default and emits the core lifecycle events. Use
`TracingVerbosity::Verbose` to include fetch/deduplication details, or `TracingVerbosity::Off`
to suppress the library's non-error tracing output.

## Process Stats

By default, the processor still publishes Sidekiq/Web-compatible process stats to Redis, including
`busy`, `beat`, `info`, and related fields. This crate no longer depends on platform-specific RSS
collection, so the published `rss` field now falls back to `"0"` on all platforms.

If you do not need Sidekiq/Web-compatible Redis process hashes, the optional `opentelemetry`
feature lets you export processor stats as OpenTelemetry gauges instead:

```rust
use opentelemetry::global;
use sidekiq::{Processor, ProcessorConfig};

let meter = global::meter("my-service");
let config = ProcessorConfig::default().opentelemetry_process_metrics(meter);

let processor = Processor::new(redis.clone(), vec!["default".to_string()]).with_config(config);
```

If you do not want either Redis process hashes or OpenTelemetry process metrics, you can also
disable process stats publishing entirely:

```rust
use sidekiq::{Processor, ProcessorConfig};

let config = ProcessorConfig::default().disable_process_stats();
let processor = Processor::new(redis.clone(), vec!["default".to_string()]).with_config(config);
```

## Runtime Requirements

- A Redis server reachable by your application.
- A Tokio runtime.
- For the integration tests in this repository, Redis is expected at `redis://127.0.0.1:6379/`.

You can start a local Redis for development with Docker:

```bash
docker run --rm -p 6379:6379 redis:7
```

And then run the full test suite:

```bash
cargo test
```


## The Worker

This library uses serde to make worker arguments strongly typed as needed. Below is an example of a worker with strongly
typed arguments. It also has custom options that will be used whenever a job is submitted. These can be overridden at
enqueue time making it easy to change the queue name, for example, should you need to.

```rust
use tracing::info;
use sidekiq::Result;

#[derive(Clone)]
struct PaymentReportWorker {}

impl PaymentReportWorker {
    fn new() -> Self {
        Self { }
    }

    async fn send_report(&self, user_guid: String) -> Result<()> {
        // TODO: Some actual work goes here...
        info!({"user_guid" = user_guid}, "Sending payment report to user");

        Ok(())
    }
}

#[derive(Deserialize, Debug, Serialize)]
struct PaymentReportArgs {
    user_guid: String,
}

#[async_trait]
impl Worker<PaymentReportArgs> for PaymentReportWorker {
    // Default worker options
    fn opts() -> sidekiq::WorkerOpts<PaymentReportArgs, Self> {
        sidekiq::WorkerOpts::new().queue("yolo")
    }

    // Worker implementation
    async fn perform(&self, args: PaymentReportArgs) -> Result<()> {
        self.send_report(args.user_guid).await
    }
}
```


## Creating a Job

There are several ways to insert a job, but for this example, we'll keep it simple. Given some worker, insert using strongly
typed arguments.

```rust
PaymentReportWorker::perform_async(
    &redis,
    PaymentReportArgs {
        user_guid: "USR-123".into(),
    },
)
.await?;
```

You can make custom overrides at enqueue time.

```rust
PaymentReportWorker::opts()
    .queue("brolo")
    .perform_async(
        &redis,
        PaymentReportArgs {
            user_guid: "USR-123".into(),
        },
    )
    .await?;
```

Or you can have more control by using the crate level method.

```rust
sidekiq::perform_async(
    &redis,
    "PaymentReportWorker".into(),
    "yolo".into(),
    PaymentReportArgs {
        user_guid: "USR-123".to_string(),
    },
)
.await?;
```

See more examples in `examples/demo.rs`.

## API Overview

The most commonly used entry points are:

- `Worker<Args>` for defining strongly typed workers.
- `Worker::perform_async` and `Worker::perform_in` for enqueueing typed jobs.
- `sidekiq::perform_async` and `sidekiq::perform_in` for enqueueing by explicit class name.
- `Processor::new(...).with_config(...)` for building a worker process.
- `periodic::builder(...)` for cron-style recurring jobs.
- `with_custom_namespace(...)` for compatibility with namespaced Redis deployments.

## Rustdoc

The public API now carries rustdoc comments in the source so `cargo doc` produces
useful crate documentation.

Generate the docs locally with:

```bash
cargo doc --no-deps
```

Then open `target/doc/sidekiq/index.html` in a browser, or use:

```bash
cargo doc --no-deps --open
```

#### Unique jobs

Unique jobs are supported via the `unique_for` option which can be defined by default on the
worker or via `SomeWorker::opts().unique_for(duration)`. See the `examples/unique.rs` example
to only enqueue a job that is unique via (worker_name, queue_name, sha256_hash_of_job_args) for
some defined `ttl`. Note: This is using `SET key value NX EX duration` under the hood as a "good
enough" lock on the job.


## Starting the Server

Below is an example of how you should create a `Processor`, register workers, include any
custom middlewares, and start the server.

```rust
// Redis
let manager = sidekiq::RedisConnectionManager::new("redis://127.0.0.1/").unwrap();
let redis = bb8::Pool::builder().build(manager).await.unwrap();

// Sidekiq server
let mut p = Processor::new(
    redis,
    vec!["yolo".to_string(), "brolo".to_string()],
);

// Add known workers
p.register(PaymentReportWorker::new());

// Custom Middlewares
p.using(FilterExpiredUsersMiddleware::new())
    .await;

// Start the server
p.run().await;
```


## Periodic Jobs

Periodic cron jobs are supported out of the box. All you need to specify is a valid
cron string and a worker instance. You can optionally supply arguments, a queue, a
retry flag, and a name that will be logged when a worker is submitted.

Example:

```rust
// Clear out all periodic jobs and their schedules
periodic::destroy_all(redis).await?;

// Add a new periodic job
periodic::builder("0 0 8 * * *")?
    .name("Email clients with an outstanding balance daily at 8am UTC")
    .queue("reminders")
    .args(EmailReminderArgs {
        report_type: "outstanding_balance",
    })?
    .register(&mut p, EmailReminderWorker)
    .await?;
```

Periodic jobs are not removed automatically. If your project adds a periodic job and
then later removes the `periodic::builder` call, the periodic job will still exist in
redis. You can call `periodic::destroy_all(redis).await?` at the start of your program
to ensure only the periodic jobs added by the latest version of your program will be
executed.

The implementation relies on a sorted set in redis. It stores a json payload of the
periodic job with a score equal to the next scheduled UTC time of the cron string. All
processes will periodically poll for changes and atomically update the score to the new
next scheduled UTC time for the cron string. The worker that successfully changes the
score atomically will enqueue a new job. Processes that don't successfully update the
score will move on. This implementation detail means periodic jobs never leave redis.
Another detail is that json when decoded and then encoded might not produce the same
value as the original string. Ex: `{"a":"b","c":"d"}` might become `{"c":"d","a":b"}`.
To keep the json representation consistent, when updating a periodic job with its new
score in redis, the original json string will be used again to keep things consistent.


## Server Middleware

One great feature of sidekiq is its middleware pattern. This library reimplements the
sidekiq server middleware pattern in rust. In the example below suppose you have an
app that performs work only for paying customers. The middleware below will halt jobs
from being executed if the customers have expired. One thing kind of interesting about
the implementation is that we can rely on serde to conditionally type-check workers.
For example, suppose I only care about user-centric workers, and I identify those by their
`user_guid` as a parameter. With serde it's easy to validate your parameters.

```rust
use tracing::info;

struct FilterExpiredUsersMiddleware {}

impl FilterExpiredUsersMiddleware {
    fn new() -> Self {
        Self { }
    }
}

#[derive(Deserialize)]
struct FilterExpiredUsersArgs {
    user_guid: String,
}

impl FilterExpiredUsersArgs {
    fn is_expired(&self) -> bool {
        self.user_guid == "USR-123-EXPIRED"
    }
}

#[async_trait]
impl ServerMiddleware for FilterExpiredUsersMiddleware {
    async fn call(
        &self,
        chain: ChainIter,
        job: &Job,
        worker: Arc<WorkerRef>,
        redis: RedisPool,
    ) -> Result<()> {
        // Use serde to check if a user_guid is part of the job args.
        let args: std::result::Result<(FilterExpiredUsersArgs,), serde_json::Error> =
            serde_json::from_value(job.args.clone());

        // If we can safely deserialize then attempt to filter based on user guid.
        if let Ok((filter,)) = args {
            if filter.is_expired() {
                error!({
                    "class" = job.class,
                    "jid" = job.jid,
                    "user_guid" = filter.user_guid },
                    "Detected an expired user, skipping this job"
                );
                return Ok(());
            }
        }

        // This customer is not expired, so we may continue.
        chain.next(job, worker, redis).await
    }
}
```

## Best practices

### Separate enqueue vs fetch connection pools

Though not required, it's recommended to use separate Redis connection pools for pushing jobs to Redis vs fetching
jobs. This has the following benefits:

- The pools can have different sizes, each optimized depending on the resource usage/constraints of your application.
- If the `sidekiq::Processor` is configured to have more worker tasks than the max size of the connection pool, then
  there may be a delay in acquiring a connection from the queue. This is a problem for enqueuing jobs, as it's normally
  desired that enqueuing be as fast as possible to avoid delaying the critical path of another operation (e.g., an API
  request). With a separate pool for enqueuing, enqueuing jobs is not impacted by the `sidekiq::Processor`'s usage of
  the pool.

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let manager = sidekiq::RedisConnectionManager::new("redis://127.0.0.1/").unwrap();
    let redis_enqueue = bb8::Pool::builder().build(manager).await.unwrap();
    let redis_fetch = bb8::Pool::builder().build(manager).await.unwrap();

    let p = Processor::new(
        redis_fetch,
        vec!["default".to_string()],
    );
    p.run().await;

    // ...

    ExampleWorker::perform_async(&redis_enqueue, ExampleArgs { foo: "bar".to_string() }).await?;

    Ok(())
}
```

## Customization Details

### Namespacing the workers

It's still very common to use the `redis-namespace` gem with ruby sidekiq workers. This library
supports namespacing redis commands by using a connection customizer when you build the connection
pool.

```rust
let manager = sidekiq::RedisConnectionManager::new("redis://127.0.0.1/")?;
let redis = bb8::Pool::builder()
    .connection_customizer(sidekiq::with_custom_namespace("my_cool_app".to_string()))
    .build(manager)
    .await?;
```

Now all Redis keys used by this library will be prefixed with `my_cool_app:`. For example, the
internal `schedule` sorted set becomes `my_cool_app:schedule`.

### Passing database connections into the workers

Workers will often need access to other software components like database connections, http clients,
etc. You can define these on your worker struct so long as they implement `Clone`. Example:

```rust
use tracing::debug;
use sidekiq::Result;

#[derive(Clone)]
struct ExampleWorker {
    redis: RedisPool,
}


#[async_trait]
impl Worker<()> for ExampleWorker {
    async fn perform(&self, args: PaymentReportArgs) -> Result<()> {
        use redis::AsyncCommands;

        // And then they are available here...
        let times_called: usize = self
            .redis
            .get()
            .await?
            .unnamespaced_borrow_mut()
            .incr("example_of_accessing_the_raw_redis_connection", 1)
            .await?;

        debug!({"times_called" = times_called}, "Called this worker");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
// ...
    let mut p = Processor::new(
        redis.clone(),
        vec!["low_priority".to_string()],
    );

    p.register(ExampleWorker{ redis: redis.clone() });
}
```

### Customizing the worker name for workers under a nested ruby module

You mind find that your worker under a module does not match with a ruby worker under a module.
A nested rusty-sidekiq worker `workers::MyWorker` will only keep the final type name `MyWorker` when
registering the worker for some "class name". Meaning, if a ruby worker is enqueued with the class
`Workers::MyWorker`, the `workers::MyWorker` type will not process that work. This is because by default
the class name is generated at compile time based on the worker struct name. To override this, redefine one
of the default trait methods:

```rust
pub struct MyWorker;
use sidekiq::Result;

#[async_trait]
impl Worker<()> for MyWorker {
    async fn perform(&self, _args: ()) -> Result<()> {
        Ok(())
    }

    fn class_name() -> String
    where
        Self: Sized,
    {
        "Workers::MyWorker".to_string()
    }
}
```

And now when ruby enqueues a `Workers::MyWorker` job, it will be picked up by rust-sidekiq.

### Customizing the number of worker tasks spawned by the `sidekiq::Processor`

If an app's workload is largely IO bound (querying a DB, making web requests and waiting for responses, etc), its
workers will spend a large percentage of time idle `await`ing for futures to complete. This in turn means the will CPU
sit idle a large percentage of the time (if nothing else is running on the host), resulting in under-utilizing available
CPU resources.

By default, the number of worker tasks spawned by the `sidekiq::Processor` is the host's CPU count, but this can
be configured depending on the needs of the app, allowing to use CPU resources more efficiently.

```rust
#[tokio::main]
async fn main() -> Result<()> {
    // ...
    let num_workers = usize::from_str(&env::var("NUM_WORKERS").unwrap()).unwrap();
    let config: ProcessorConfig = Default::default();
    let config = config.num_workers(num_workers);
    let processor = Processor::new(redis_fetch, queues.clone())
        .with_config(config);
    // ...
}
```

## License

MIT
