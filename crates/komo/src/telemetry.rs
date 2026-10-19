//! OTLP 导出（docs/observability.md §4）。**只在 Gateway 进程里建**；CLI 命令从不导出。
//!
//! 导出走 SDK 自己的后台线程（批量 span 处理器、周期指标读取器），队列有上限、满了丢弃，
//! `on_end` 不阻塞调用方——后端不通拖不慢任何一个 Run。HTTP 客户端用 reqwest 的 blocking
//! 版：SDK 在无 tokio reactor 的线程上 `block_on` 导出 future，async 版 reqwest 在那里会
//! panic。

use std::sync::Arc;
use std::time::Duration;

use komo_gateway::TelemetryStatus;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{
    MetricExporter, Protocol, SpanExporter as OtlpSpanExporter, WithExportConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter, Tracer};
use time::OffsetDateTime;
use tracing::Subscriber;
use tracing_opentelemetry::{MetricsLayer, OpenTelemetryLayer};
use tracing_subscriber::registry::LookupSpan;

const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// OTel 两层各自的过滤，与 `KOMO_LOG` 无关：span 取 INFO 及以上，`komo::metrics` 的 DEBUG
/// 指标事件放行（埋点约定见 docs/observability.md §4）。
pub fn export_filter() -> tracing_subscriber::filter::Targets {
    tracing_subscriber::filter::Targets::new()
        .with_default(tracing::Level::INFO)
        .with_target("komo::metrics", tracing::Level::DEBUG)
}

pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    tracer: Tracer,
}

impl Telemetry {
    /// 建导出管线。`endpoint` 是 OTLP/HTTP 的根（`http://127.0.0.1:4318`），信号路径在这里补。
    pub fn build(
        endpoint: &str,
        instance_id: &str,
        status: Arc<TelemetryStatus>,
    ) -> Result<Self, String> {
        let base = endpoint.trim_end_matches('/');
        let resource = Resource::builder()
            .with_service_name("komo")
            .with_attributes([
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                KeyValue::new("service.instance.id", instance_id.to_string()),
            ])
            .build();

        let spans = OtlpSpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{base}/v1/traces"))
            .with_timeout(EXPORT_TIMEOUT)
            .build()
            .map_err(|error| error.to_string())?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(ReportingExporter {
                inner: spans,
                status,
            })
            .build();

        let metrics = MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{base}/v1/metrics"))
            .with_timeout(EXPORT_TIMEOUT)
            .build()
            .map_err(|error| error.to_string())?;
        let meter_provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(PeriodicReader::builder(metrics).build())
            .build();

        let tracer = tracer_provider.tracer("komo");
        Ok(Self {
            tracer_provider,
            meter_provider,
            tracer,
        })
    }

    /// 两层各自要对着"它下面那一层"定型，所以分开给。
    pub fn trace_layer<S>(&self) -> OpenTelemetryLayer<S, Tracer>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
    {
        OpenTelemetryLayer::new(self.tracer.clone())
    }

    pub fn metrics_layer<S>(&self) -> MetricsLayer<S, SdkMeterProvider>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
    {
        MetricsLayer::new(self.meter_provider.clone())
    }

    /// 退出前把队列里的送出去；后端不通时最多等 [`SHUTDOWN_TIMEOUT`]。
    ///
    /// 阻塞调用（要 join SDK 线程），async 里用 `spawn_blocking`。
    pub fn shutdown(&self) {
        let _ = self.tracer_provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
        let _ = self.meter_provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
    }
}

/// 包一层 span exporter，把每批的结果记进共享状态：第一次失败 warn 一条，恢复 info 一条，
/// 其余静默。
#[derive(Debug)]
struct ReportingExporter<E> {
    inner: E,
    status: Arc<TelemetryStatus>,
}

impl<E: SpanExporter> SpanExporter for ReportingExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        let now = OffsetDateTime::now_utc();
        match &result {
            Ok(()) => {
                if self.status.record_ok(now) {
                    tracing::info!("遥测导出已恢复");
                }
            }
            Err(error) => {
                if self.status.record_err(now, error.to_string()) {
                    tracing::warn!(%error, "遥测导出失败（之后的失败不再重复记录，恢复时记一条）");
                }
            }
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt;

    /// 后端不通（本机一个没人听的端口）：建管线、发一千个 span 都不等网络，
    /// flush 之后失败被记进状态，而且只在第一次是"边沿"。
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_endpoint_never_blocks_the_emitter() {
        let endpoint = "http://127.0.0.1:9";
        let status = Arc::new(TelemetryStatus::new(Some(endpoint.into())));
        let started = Instant::now();
        let telemetry = Telemetry::build(endpoint, "test-host", Arc::clone(&status)).unwrap();
        let subscriber = tracing_subscriber::registry()
            .with(telemetry.trace_layer().with_filter(export_filter()))
            .with(telemetry.metrics_layer().with_filter(export_filter()));
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..1000 {
                let span = tracing::info_span!("probe", i);
                let _enter = span.enter();
                tracing::event!(target: "komo::metrics", tracing::Level::DEBUG, histogram.probe_seconds = 0.1_f64, "tick");
            }
        });
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "发 span 不该等网络：{:?}",
            started.elapsed()
        );

        let flushed = Instant::now();
        let tracer = telemetry.tracer_provider.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tracer.force_flush();
        })
        .await
        .unwrap();
        assert!(flushed.elapsed() < Duration::from_secs(5));
        let health = status.health();
        assert!(health.last_error_at.is_some(), "{health:?}");
        assert_eq!(health.last_ok_at, None);

        tokio::task::spawn_blocking(move || telemetry.shutdown())
            .await
            .unwrap();
    }
}
