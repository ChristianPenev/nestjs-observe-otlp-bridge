//! Observe metrics into OTLP metrics.
//!
//! Counters are **delta** temporality, because both kinds the SDK sends already are:
//! GC figures are zeroed every collection window, and a custom counter's `increase`
//! is what it rose by since the last flush. Calling them cumulative would make a
//! backend read each window as a running total that keeps collapsing to zero.
//!
//! Gauges carry no temporality, so nothing has to be decided for them.

use chrono::{DateTime, Utc};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Gauge, Metric as OtlpMetric, NumberDataPoint, ResourceMetrics,
    ScopeMetrics, Sum, metric, number_data_point,
};

use crate::mapping::attributes::AttributeBuilder;
use crate::mapping::traces::{resource, scope};
use crate::observe::model::{Batch, Metric, MetricKind, MetricValue};

/// Builds the `ResourceMetrics` for a batch, or `None` when it carried no metrics.
///
/// `received_at` dates any point the SDK sent without a timestamp - the runtime
/// gauges, which are sampled on an interval and reported without one.
pub fn map(batch: &Batch, received_at: DateTime<Utc>) -> Option<ResourceMetrics> {
    if batch.metrics.is_empty() {
        return None;
    }

    Some(ResourceMetrics {
        resource: Some(resource(&batch.service)),
        scope_metrics: vec![ScopeMetrics {
            scope: Some(scope()),
            metrics: batch
                .metrics
                .iter()
                .map(|metric| map_metric(metric, received_at))
                .collect(),
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    })
}

fn map_metric(metric: &Metric, received_at: DateTime<Utc>) -> OtlpMetric {
    let timestamp = to_nanos(metric.timestamp.unwrap_or(received_at));

    let mut builder = AttributeBuilder::new();
    builder.tags(&metric.attributes);

    let point = NumberDataPoint {
        attributes: builder.build(),
        // The SDK reports a reading, not a window, so start and end coincide. A
        // backend computing a rate uses the gap between successive points.
        start_time_unix_nano: timestamp,
        time_unix_nano: timestamp,
        value: Some(match metric.value {
            MetricValue::F64(value) => number_data_point::Value::AsDouble(value),
        }),
        ..Default::default()
    };

    let data = match metric.kind {
        MetricKind::Gauge => metric::Data::Gauge(Gauge {
            data_points: vec![point],
        }),
        MetricKind::Counter => metric::Data::Sum(Sum {
            data_points: vec![point],
            aggregation_temporality: AggregationTemporality::Delta as i32,
            is_monotonic: true,
        }),
    };

    OtlpMetric {
        name: metric.name.clone(),
        description: metric.description.clone().unwrap_or_default(),
        unit: metric.unit.unwrap_or_default().to_string(),
        data: Some(data),
        ..Default::default()
    }
}

fn to_nanos(timestamp: DateTime<Utc>) -> u64 {
    timestamp.timestamp_nanos_opt().unwrap_or(0).max(0) as u64
}

#[cfg(test)]
mod tests;
