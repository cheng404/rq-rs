use crate::RedisPool;
#[cfg(feature = "opentelemetry")]
use opentelemetry::{
    metrics::{Gauge, Meter},
    KeyValue,
};
use rand::RngCore;
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use sysinfo::{get_current_pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Clone)]
/// Thread-safe counter used for tracking processor state.
pub struct Counter {
    count: Arc<AtomicUsize>,
}

impl Counter {
    #[must_use]
    /// Create a new counter with the provided initial value.
    pub fn new(n: usize) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(n)),
        }
    }

    #[must_use]
    /// Read the current counter value.
    pub fn value(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Decrement the counter by `n`.
    pub fn decrby(&self, n: usize) {
        self.count.fetch_sub(n, Ordering::SeqCst);
    }

    /// Increment the counter by `n`.
    pub fn incrby(&self, n: usize) {
        self.count.fetch_add(n, Ordering::SeqCst);
    }
}

struct ProcessStats {
    rtt_us: String,
    quiet: bool,
    busy: usize,
    beat: chrono::DateTime<chrono::Utc>,
    info: ProcessInfo,
    rss: String,
}

#[derive(Serialize)]
struct ProcessInfo {
    hostname: String,
    identity: String,
    started_at: f64,
    pid: u32,
    tag: Option<String>,
    concurrency: usize,
    queues: Vec<String>,
    labels: Vec<String>,
}

/// Publishes Sidekiq-compatible process statistics to Redis.
pub struct StatsPublisher {
    hostname: String,
    identity: String,
    queues: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    busy_jobs: Counter,
    concurrency: usize,
}

fn generate_identity(hostname: &String) -> String {
    let pid = std::process::id();
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    let nonce = hex::encode(bytes);

    format!("{hostname}:{pid}:{nonce}")
}

const BYTES_PER_KIB: u64 = 1024;

fn current_process_rss_kb() -> String {
    let Ok(pid) = get_current_pid() else {
        return "0".to_string();
    };

    let system = PROCESS_MEMORY_SYSTEM.get_or_init(|| Mutex::new(System::new()));
    let mut system = system.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_memory(),
    );

    system
        .process(pid)
        .map(|process| (process.memory() / BYTES_PER_KIB).to_string())
        .unwrap_or_else(|| "0".to_string())
}

static PROCESS_MEMORY_SYSTEM: OnceLock<Mutex<System>> = OnceLock::new();

impl StatsPublisher {
    #[must_use]
    /// Create a new stats publisher for a processor instance.
    pub fn new(
        hostname: String,
        queues: Vec<String>,
        busy_jobs: Counter,
        concurrency: usize,
    ) -> Self {
        let identity = generate_identity(&hostname);
        let started_at = chrono::Utc::now();

        Self {
            hostname,
            identity,
            queues,
            started_at,
            busy_jobs,
            concurrency,
        }
    }

    // 127.0.0.1:6379> hkeys "yolo_app:DESKTOP-UMSV21A:107068:5075431aeb06"
    // 1) "rtt_us"
    // 2) "quiet"
    // 3) "busy"
    // 4) "beat"
    // 5) "info"
    // 6) "rss"
    // 127.0.0.1:6379> hget "yolo_app:DESKTOP-UMSV21A:107068:5075431aeb06" info
    // "{\"hostname\":\"DESKTOP-UMSV21A\",\"started_at\":1658082501.5606177,\"pid\":107068,\"tag\":\"\",\"concurrency\":10,\"queues\":[\"ruby:v1_statistics\",\"ruby:v2_statistics\"],\"labels\":[],\"identity\":\"DESKTOP-UMSV21A:107068:5075431aeb06\"}"
    // 127.0.0.1:6379> hget "yolo_app:DESKTOP-UMSV21A:107068:5075431aeb06" irss
    // (nil)
    /// Publish one heartbeat payload to Redis.
    pub async fn publish_stats(&self, redis: RedisPool) -> Result<(), Box<dyn std::error::Error>> {
        let stats = self.create_process_stats().await?;
        let mut conn = redis.get().await?;
        let _: () = conn
            .cmd_with_key("HSET", self.identity.clone())
            .arg("rss")
            .arg(stats.rss)
            .arg("rtt_us")
            .arg(stats.rtt_us)
            .arg("busy")
            .arg(stats.busy)
            .arg("quiet")
            .arg(stats.quiet)
            .arg("beat")
            .arg(stats.beat.timestamp())
            .arg("info")
            .arg(serde_json::to_string(&stats.info)?)
            .query_async::<()>(conn.unnamespaced_borrow_mut())
            .await?;

        conn.expire(self.identity.clone(), 30).await?;

        conn.sadd("processes".to_string(), self.identity.clone())
            .await?;

        Ok(())
    }

    async fn create_process_stats(&self) -> Result<ProcessStats, Box<dyn std::error::Error>> {
        Ok(ProcessStats {
            rtt_us: "0".into(),
            busy: self.busy_jobs.value(),
            quiet: false,
            rss: current_process_rss_kb(),

            beat: chrono::Utc::now(),
            info: ProcessInfo {
                concurrency: self.concurrency,
                hostname: self.hostname.clone(),
                identity: self.identity.clone(),
                queues: self.queues.clone(),
                started_at: self.started_at.clone().timestamp() as f64,
                pid: std::process::id(),

                // TODO: Fill out labels and tags.
                labels: vec![],
                tag: None,
            },
        })
    }
}

#[cfg(feature = "opentelemetry")]
/// Publishes processor process statistics to OpenTelemetry metrics.
pub struct OpenTelemetryStatsPublisher {
    started_at: chrono::DateTime<chrono::Utc>,
    busy_job_counter: Counter,
    concurrency: usize,
    attributes: Vec<KeyValue>,
    info: Gauge<u64>,
    heartbeat: Gauge<u64>,
    busy: Gauge<u64>,
    concurrency_limit: Gauge<u64>,
    started_at_unix: Gauge<u64>,
}

#[cfg(feature = "opentelemetry")]
impl OpenTelemetryStatsPublisher {
    #[must_use]
    /// Create a new OpenTelemetry-backed stats publisher for a processor instance.
    pub fn new(
        hostname: String,
        queues: Vec<String>,
        busy_jobs: Counter,
        concurrency: usize,
        meter: Meter,
    ) -> Self {
        let identity = generate_identity(&hostname);
        let started_at = chrono::Utc::now();
        let pid = std::process::id();
        let attributes = vec![
            KeyValue::new("hostname", hostname),
            KeyValue::new("identity", identity),
            KeyValue::new("pid", i64::from(pid)),
            KeyValue::new("queues", queues.join(",")),
        ];

        Self {
            started_at,
            busy_job_counter: busy_jobs,
            concurrency,
            attributes,
            info: meter
                .u64_gauge("rusty_sidekiq_processor_info")
                .with_description("Static metadata for a rusty-sidekiq processor instance.")
                .build(),
            heartbeat: meter
                .u64_gauge("rusty_sidekiq_processor_heartbeat_unix_time")
                .with_unit("s")
                .with_description("Unix timestamp of the most recent processor heartbeat.")
                .build(),
            busy: meter
                .u64_gauge("rusty_sidekiq_processor_busy_jobs")
                .with_description("Number of jobs currently being processed by this processor.")
                .build(),
            concurrency_limit: meter
                .u64_gauge("rusty_sidekiq_processor_concurrency")
                .with_description("Configured worker concurrency for this processor.")
                .build(),
            started_at_unix: meter
                .u64_gauge("rusty_sidekiq_processor_started_at_unix_time")
                .with_unit("s")
                .with_description("Unix timestamp of when this processor instance started.")
                .build(),
        }
    }

    /// Publish one heartbeat payload to OpenTelemetry.
    pub async fn publish_stats(&self) -> Result<(), Box<dyn std::error::Error>> {
        let attributes = self.attributes.as_slice();
        self.info.record(1, attributes);
        self.heartbeat
            .record(chrono::Utc::now().timestamp() as u64, attributes);
        self.busy
            .record(self.busy_job_counter.value() as u64, attributes);
        self.concurrency_limit
            .record(self.concurrency as u64, attributes);
        self.started_at_unix
            .record(self.started_at.timestamp() as u64, attributes);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::current_process_rss_kb;

    #[test]
    fn collects_current_process_rss_in_kb() {
        let rss_kb = current_process_rss_kb()
            .parse::<u64>()
            .expect("rss should be a valid integer");

        assert!(rss_kb > 0, "rss should be greater than zero");
    }
}

#[cfg(all(test, feature = "opentelemetry"))]
mod opentelemetry_tests {
    use super::{Counter, OpenTelemetryStatsPublisher};
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::metrics::{
        data::{AggregatedMetrics, MetricData, ResourceMetrics},
        reader::MetricReader,
        InstrumentKind, ManualReader, Pipeline, SdkMeterProvider, Temporality,
    };
    use std::{
        sync::{Arc, Weak},
        time::Duration,
    };

    #[derive(Clone, Debug)]
    struct SharedReader(Arc<dyn MetricReader>);

    impl MetricReader for SharedReader {
        fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
            self.0.register_pipeline(pipeline)
        }

        fn collect(&self, rm: &mut ResourceMetrics) -> OTelSdkResult {
            self.0.collect(rm)
        }

        fn force_flush(&self) -> OTelSdkResult {
            self.0.force_flush()
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            self.0.shutdown()
        }

        fn temporality(&self, kind: InstrumentKind) -> Temporality {
            self.0.temporality(kind)
        }
    }

    fn read_gauge_value(metrics: &ResourceMetrics, name: &str) -> Option<u64> {
        metrics.scope_metrics().find_map(|scope_metrics| {
            scope_metrics.metrics().find_map(|metric| {
                if metric.name() != name {
                    return None;
                }

                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                        gauge.data_points().next().map(|point| point.value())
                    }
                    _ => None,
                }
            })
        })
    }

    #[tokio::test]
    async fn open_telemetry_stats_publisher_records_processor_metrics() {
        let reader = SharedReader(Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        let meter = provider.meter("rq-rs-test");

        let busy_jobs = Counter::new(0);
        busy_jobs.incrby(3);
        let publisher = OpenTelemetryStatsPublisher::new(
            "test-host".to_string(),
            vec!["default".to_string(), "critical".to_string()],
            busy_jobs,
            8,
            meter,
        );

        publisher.publish_stats().await.unwrap();

        let mut metrics = ResourceMetrics::default();
        reader.collect(&mut metrics).unwrap();

        assert_eq!(
            read_gauge_value(&metrics, "rusty_sidekiq_processor_info"),
            Some(1)
        );
        assert_eq!(
            read_gauge_value(&metrics, "rusty_sidekiq_processor_busy_jobs"),
            Some(3)
        );
        assert_eq!(
            read_gauge_value(&metrics, "rusty_sidekiq_processor_concurrency"),
            Some(8)
        );
        assert!(
            read_gauge_value(&metrics, "rusty_sidekiq_processor_started_at_unix_time")
                .unwrap_or_default()
                > 0
        );
        assert!(
            read_gauge_value(&metrics, "rusty_sidekiq_processor_heartbeat_unix_time")
                .unwrap_or_default()
                > 0
        );
    }
}
