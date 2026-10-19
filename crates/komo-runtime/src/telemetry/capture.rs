//! 测试用的 `tracing` 层：把 span（名字、父子、属性）与事件（target、字段）收进内存，
//! 供断言。只在 `cfg(test)` 下编译；init 侧的 OpenTelemetry 层在 bin 里，这里只验埋点。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

#[derive(Debug, Clone)]
pub struct SpanRec {
    pub id: u64,
    pub parent: Option<u64>,
    pub name: &'static str,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct EventRec {
    pub target: String,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct Inner {
    spans: Vec<SpanRec>,
    events: Vec<EventRec>,
    /// 活着的 span：tracing 的 id 会在关闭后复用，所以记录用自己的序号。
    live: std::collections::HashMap<u64, u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Capture(Arc<Mutex<Inner>>);

impl Capture {
    /// 装成当前线程的默认订阅者；守卫 drop 时撤掉。
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
    }

    pub fn spans(&self, name: &str) -> Vec<SpanRec> {
        let inner = self.0.lock().unwrap();
        inner
            .spans
            .iter()
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }

    pub fn children(&self, parent: &SpanRec, name: &str) -> Vec<SpanRec> {
        let inner = self.0.lock().unwrap();
        inner
            .spans
            .iter()
            .filter(|span| span.name == name && span.parent == Some(parent.id))
            .cloned()
            .collect()
    }

    pub fn all_spans(&self) -> Vec<SpanRec> {
        self.0.lock().unwrap().spans.clone()
    }

    /// `komo::metrics` 事件里带这个前缀字段（`histogram.` / `monotonic_counter.`）的。
    pub fn metrics(&self, field: &str) -> Vec<EventRec> {
        let inner = self.0.lock().unwrap();
        inner
            .events
            .iter()
            .filter(|event| event.target == super::METRICS && event.fields.contains_key(field))
            .cloned()
            .collect()
    }

    pub fn all_metrics(&self) -> Vec<EventRec> {
        let inner = self.0.lock().unwrap();
        inner
            .events
            .iter()
            .filter(|event| event.target == super::METRICS)
            .cloned()
            .collect()
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S> tracing_subscriber::Layer<S> for Capture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let mut inner = self.0.lock().unwrap();
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .and_then(|parent| inner.live.get(&parent.id().into_u64()).copied());
        let number = inner.spans.len() as u64;
        inner.live.insert(id.into_u64(), number);
        inner.spans.push(SpanRec {
            id: number,
            parent,
            name: attrs.metadata().name(),
            fields: fields.0,
        });
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        self.0.lock().unwrap().live.remove(&id.into_u64());
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        values.record(&mut fields);
        let mut inner = self.0.lock().unwrap();
        if let Some(number) = inner.live.get(&id.into_u64()).copied() {
            inner.spans[number as usize].fields.extend(fields.0);
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().unwrap().events.push(EventRec {
            target: event.metadata().target().to_string(),
            fields: fields.0,
        });
    }
}
