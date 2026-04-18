use async_trait::async_trait;
use bb8::Pool;
use serial_test::serial;
use sidekiq::{
    set_tracing_config, Processor, RedisConnectionManager, RedisPool, Result, TracingConfig,
    TracingVerbosity, WorkFetcher, Worker,
};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;

#[async_trait]
trait FlushAll {
    async fn flushall(&self);
}

#[async_trait]
impl FlushAll for RedisPool {
    async fn flushall(&self) {
        let mut conn = self.get().await.unwrap();
        let _: String = redis::cmd("FLUSHALL")
            .query_async(conn.unnamespaced_borrow_mut())
            .await
            .unwrap();
    }
}

#[derive(Clone, Default)]
struct SharedWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn into_string(&self) -> String {
        String::from_utf8(self.buffer.lock().unwrap().clone()).unwrap()
    }
}

struct SharedWriterGuard {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedWriterGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriterGuard {
            buffer: self.buffer.clone(),
        }
    }
}

fn tracing_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone)]
struct TestWorker;

#[async_trait]
impl Worker<()> for TestWorker {
    async fn perform(&self, _args: ()) -> Result<()> {
        Ok(())
    }
}

fn with_captured_tracing(
    verbosity: TracingVerbosity,
    f: impl FnOnce() -> String,
) -> String {
    let _lock = tracing_test_lock().lock().unwrap();
    let writer = SharedWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::DEBUG)
        .with_current_span(true)
        .with_span_events(FmtSpan::NEW)
        .with_writer(writer.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);

    set_tracing_config(TracingConfig::default().verbosity(verbosity));
    let result = tracing::dispatcher::with_default(&dispatch, f);
    set_tracing_config(TracingConfig::default());

    if result.is_empty() {
        return writer.into_string();
    }

    format!("{}{}", writer.into_string(), result)
}

#[test]
#[serial]
fn emits_job_spans_and_events_when_verbose_tracing_is_enabled() {
    let output = with_captured_tracing(TracingVerbosity::Verbose, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let manager = RedisConnectionManager::new("redis://127.0.0.1/").unwrap();
            let redis = Pool::builder().build(manager).await.unwrap();
            redis.flushall().await;

            let queue = "trace_queue".to_string();
            let mut processor = Processor::new(redis.clone(), vec![queue.clone()]);
            processor.register(TestWorker);

            TestWorker::opts()
                .queue(queue)
                .perform_async(&redis, ())
                .await
                .unwrap();

            assert_eq!(processor.process_one_tick_once().await.unwrap(), WorkFetcher::Done);
        });

        String::new()
    });

    assert!(output.contains("sidekiq.job"));
    assert!(output.contains("job.enqueued"));
    assert!(output.contains("job.fetched"));
    assert!(output.contains("job.started"));
    assert!(output.contains("job.completed"));
    assert!(output.contains("\"class\":\"TestWorker\""));
    assert!(output.contains("\"queue\":\"trace_queue\""));
    assert!(output.contains("\"jid\":\""));
    assert!(output.contains("\"worker_type\":\"worker\""));
    assert!(output.contains("\"elapsed\":"));
}

#[test]
#[serial]
fn suppresses_lifecycle_events_when_tracing_is_off() {
    let output = with_captured_tracing(TracingVerbosity::Off, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let manager = RedisConnectionManager::new("redis://127.0.0.1/").unwrap();
            let redis = Pool::builder().build(manager).await.unwrap();
            redis.flushall().await;

            let queue = "trace_queue_off".to_string();
            let mut processor = Processor::new(redis.clone(), vec![queue.clone()]);
            processor.register(TestWorker);

            TestWorker::opts()
                .queue(queue)
                .perform_async(&redis, ())
                .await
                .unwrap();

            assert_eq!(processor.process_one_tick_once().await.unwrap(), WorkFetcher::Done);
        });

        String::new()
    });

    assert!(!output.contains("job.enqueued"));
    assert!(!output.contains("job.started"));
    assert!(!output.contains("job.completed"));
}
