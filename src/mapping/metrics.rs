//! Observe metrics into OTLP metrics.
//!
//! Counters are exported with **delta** temporality. Both kinds of counter the SDK
//! sends are already deltas: the runtime GC figures are zeroed after every
//! collection window, and a custom counter's `increase` is what it rose by since the
//! last successful flush. Declaring them cumulative would make a backend read each
//! window's figure as a running total that keeps falling back to near zero.
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
mod tests {
    use super::*;
    use crate::observe::decoder;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn mapped(payload: &str) -> Vec<OtlpMetric> {
        let batch = decoder::decode(payload.as_bytes(), false).unwrap();
        map(&batch, at("2026-10-01T12:00:00Z"))
            .map(|metrics| metrics.scope_metrics[0].metrics.clone())
            .unwrap_or_default()
    }

    fn find<'a>(metrics: &'a [OtlpMetric], name: &str) -> &'a OtlpMetric {
        metrics
            .iter()
            .find(|metric| metric.name == name)
            .unwrap_or_else(|| panic!("no metric named {name}"))
    }

    fn double(metric: &OtlpMetric) -> f64 {
        let point = match metric.data.as_ref().unwrap() {
            metric::Data::Gauge(gauge) => &gauge.data_points[0],
            metric::Data::Sum(sum) => &sum.data_points[0],
            _ => panic!("unexpected metric type"),
        };
        match point.value.as_ref().unwrap() {
            number_data_point::Value::AsDouble(value) => *value,
            number_data_point::Value::AsInt(value) => *value as f64,
        }
    }

    #[test]
    fn runtime_gauges_carry_their_units() {
        let metrics = mapped(r#"{"serviceId":"api","runtime":{"m":{"hu":64.0},"e":{"l":1.5}}}"#);
        let heap = find(&metrics, "nestjs.runtime.memory.heap.used");
        assert_eq!(heap.unit, "MBy");
        assert_eq!(double(heap), 64.0);
        assert!(matches!(
            heap.data.as_ref().unwrap(),
            metric::Data::Gauge(_)
        ));

        let lag = find(&metrics, "nestjs.runtime.event_loop.delay");
        assert_eq!(lag.unit, "ms");
    }

    #[test]
    fn gc_counters_are_delta_sums() {
        let metrics = mapped(r#"{"serviceId":"api","runtime":{"g":{"c":3,"td":12.5}}}"#);
        let collections = find(&metrics, "nestjs.runtime.gc.collections");
        let metric::Data::Sum(sum) = collections.data.as_ref().unwrap() else {
            panic!("expected a sum");
        };
        assert_eq!(
            sum.aggregation_temporality,
            AggregationTemporality::Delta as i32
        );
        assert!(sum.is_monotonic);
    }

    #[test]
    fn a_custom_counter_is_a_delta_sum_of_its_increase() {
        let metrics = mapped(
            r#"{"serviceId":"api","custom":[
                {"n":"orders","t":"counter","v":{"default":500},"iv":{"default":12}}
            ]}"#,
        );
        let orders = find(&metrics, "nestjs.custom.orders");
        assert_eq!(double(orders), 12.0);
        let metric::Data::Sum(sum) = orders.data.as_ref().unwrap() else {
            panic!("expected a sum");
        };
        assert_eq!(
            sum.aggregation_temporality,
            AggregationTemporality::Delta as i32
        );
    }

    #[test]
    fn a_labelled_counter_produces_a_point_per_label_set() {
        let metrics = mapped(
            r#"{"serviceId":"api","custom":[
                {"n":"logins","t":"counter",
                 "iv":{"{\"route\":\"/login\"}":3,"{\"route\":\"/sso\"}":5}}
            ]}"#,
        );
        let logins: Vec<_> = metrics
            .iter()
            .filter(|metric| metric.name == "nestjs.custom.logins")
            .collect();
        assert_eq!(logins.len(), 2);
        // Each stream carries the decoded label as a real attribute.
        assert!(logins.iter().all(|metric| {
            let metric::Data::Sum(sum) = metric.data.as_ref().unwrap() else {
                return false;
            };
            sum.data_points[0]
                .attributes
                .iter()
                .any(|entry| entry.key == "route")
        }));
    }

    #[test]
    fn a_gauge_is_not_a_sum() {
        let metrics = mapped(
            r#"{"serviceId":"api","custom":[{"n":"depth","t":"gauge","v":{"default":17}}]}"#,
        );
        assert!(matches!(
            find(&metrics, "nestjs.custom.depth").data.as_ref().unwrap(),
            metric::Data::Gauge(_)
        ));
    }

    #[test]
    fn a_point_without_a_timestamp_is_dated_at_receipt() {
        let metrics = mapped(r#"{"serviceId":"api","runtime":{"m":{"hu":1.0}}}"#);
        let expected = at("2026-10-01T12:00:00Z").timestamp_nanos_opt().unwrap() as u64;
        let metric::Data::Gauge(gauge) = metrics[0].data.as_ref().unwrap() else {
            panic!("expected a gauge");
        };
        assert_eq!(gauge.data_points[0].time_unix_nano, expected);
    }

    #[test]
    fn a_metrics_own_timestamp_wins() {
        let metrics = mapped(
            r#"{"serviceId":"api","custom":[
                {"n":"x","t":"gauge","v":{"default":1},"lu":1759312800000}
            ]}"#,
        );
        let metric::Data::Gauge(gauge) = metrics[0].data.as_ref().unwrap() else {
            panic!("expected a gauge");
        };
        assert_eq!(
            gauge.data_points[0].time_unix_nano,
            1_759_312_800_000_000_000
        );
    }

    #[test]
    fn a_batch_with_no_metrics_maps_to_nothing() {
        let batch = decoder::decode(br#"{"serviceId":"api"}"#, false).unwrap();
        assert!(map(&batch, at("2026-10-01T12:00:00Z")).is_none());
    }
}
