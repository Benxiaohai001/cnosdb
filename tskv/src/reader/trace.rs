use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::RecordBatch;
use datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricValue, MetricsSet};
use futures::{Stream, StreamExt};
use trace::span_ext::SpanExt;
use trace::{Span, SpanContext};

use super::{
    BatchReader, BatchReaderRef, SchemableTskvRecordBatchStream,
    SendableSchemableTskvRecordBatchStream,
};
use crate::TskvResult;

pub struct TraceCollectorBatcherReaderProxy {
    inner: BatchReaderRef,
    span: Span,
    metrics_sets: HashMap<String, Arc<ExecutionPlanMetricsSet>>,
}

impl TraceCollectorBatcherReaderProxy {
    pub fn new(inner: BatchReaderRef, span: Span) -> Self {
        Self {
            inner,
            span,
            metrics_sets: HashMap::default(),
        }
    }

    pub fn register_metrics_set(
        mut self,
        name: impl Into<String>,
        metrics_set: Arc<ExecutionPlanMetricsSet>,
    ) -> Self {
        self.metrics_sets.insert(name.into(), metrics_set);
        self
    }
}

impl BatchReader for TraceCollectorBatcherReaderProxy {
    fn process(&self) -> TskvResult<SendableSchemableTskvRecordBatchStream> {
        let input = self.inner.process()?;

        // 如果开启了 trace，则将 input 包装成 TraceCollectorStream 用于采集 trace 信息
        if SpanContext::from_span(&self.span).is_some() {
            return Ok(Box::pin(TraceCollectorStream {
                inner: input,
                span: Span::enter_with_parent("TraceCollectorStream", &self.span),
                metrics_sets: self.metrics_sets.clone(),
            }));
        }

        Ok(input)
    }

    fn fmt_as(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.inner.fmt_as(f)
    }

    fn children(&self) -> Vec<BatchReaderRef> {
        self.inner.children()
    }
}

struct TraceCollectorStream {
    inner: SendableSchemableTskvRecordBatchStream,
    span: Span,
    metrics_sets: HashMap<String, Arc<ExecutionPlanMetricsSet>>,
}

impl SchemableTskvRecordBatchStream for TraceCollectorStream {
    fn schema(&self) -> arrow::datatypes::SchemaRef {
        self.inner.schema()
    }
}

impl Stream for TraceCollectorStream {
    type Item = TskvResult<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl Drop for TraceCollectorStream {
    fn drop(&mut self) {
        if SpanContext::from_span(&self.span).is_some() {
            for (name, metrics) in &self.metrics_sets {
                metrics
                    .clone_inner()
                    .record(&mut self.span, name.to_string());
            }
        }
    }
}

pub trait Recorder {
    fn record(&self, span: &mut Span, name: impl Into<Cow<'static, str>>);
}

impl Recorder for MetricsSet {
    fn record(&self, span: &mut Span, name: impl Into<Cow<'static, str>>) {
        if self.iter().size_hint().0 == 0 {
            return;
        }

        let start_ts = self
            .iter()
            .filter_map(|e| match e.value() {
                MetricValue::StartTimestamp(ts) => ts.value(),
                _ => None,
            })
            .min();
        let end_ts = self
            .iter()
            .filter_map(|e| match e.value() {
                MetricValue::EndTimestamp(ts) => ts.value(),
                _ => None,
            })
            .min();
        let metrics = self
            .aggregate_by_name()
            .sorted_for_display()
            .timestamps_removed();

        span.add_property(|| {
            (
                name,
                format!(
                    "{}, start_ts={}. end_ts={}",
                    metrics,
                    start_ts.unwrap_or_default(),
                    end_ts.unwrap_or_default(),
                ),
            )
        });
    }
}
