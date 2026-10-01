//! The metric half of decoding.
//!
//! Observe sends measurements in two unrelated shapes: `runtime` is one nested
//! object the agent samples on a timer, and `custom` is a list of records the
//! application declared, each with its own per-label series. OTLP wants neither - it
//! wants one stream per measurement - so both are flattened here, into the same
//! `Metric`. Doing it at decode time is what keeps `mapping::metrics` from having to
//! know which section a number arrived in.

use serde_json::Value;

use super::{from_millis, to_attributes};
use crate::observe::model::{Attributes, Metric, MetricKind, MetricValue};
use crate::observe::protocol;

/// Splits the nested runtime object into one stream per measurement.
///
/// The counters here - GC count and duration - are *deltas*: the SDK zeroes them
/// after every collection window, so they must be exported with delta temporality
/// rather than as cumulative totals, or a backend will read each window's figure as
/// a running total that keeps resetting.
pub(super) fn push_runtime_metrics(runtime: &protocol::RuntimeMetrics, out: &mut Vec<Metric>) {
    let gauge = |name: &str, unit: Option<&'static str>, value: Option<f64>| {
        value.map(|value| Metric {
            name: name.to_string(),
            description: None,
            unit,
            kind: MetricKind::Gauge,
            value: MetricValue::F64(value),
            attributes: Attributes::new(),
            timestamp: None,
        })
    };

    if let Some(cpu) = &runtime.cpu {
        out.extend(gauge("nestjs.runtime.cpu.user", Some("ms"), cpu.user));
        out.extend(gauge("nestjs.runtime.cpu.system", Some("ms"), cpu.system));
        out.extend(gauge(
            "nestjs.runtime.cpu.utilization",
            Some("%"),
            cpu.percentage_used,
        ));
    }

    if let Some(memory) = &runtime.memory {
        // Megabytes, not bytes: `NodeRuntimeMetricsService` divides by 1024 twice
        // before sending. Reporting `By` here would overstate memory by 10^6.
        out.extend(gauge("nestjs.runtime.memory.rss", Some("MBy"), memory.rss));
        out.extend(gauge(
            "nestjs.runtime.memory.heap.total",
            Some("MBy"),
            memory.heap_total,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.heap.used",
            Some("MBy"),
            memory.heap_used,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.external",
            Some("MBy"),
            memory.external,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.array_buffers",
            Some("MBy"),
            memory.array_buffers,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.utilization",
            Some("%"),
            memory.percentage_used,
        ));
    }

    if let Some(event_loop) = &runtime.event_loop {
        out.extend(gauge(
            "nestjs.runtime.event_loop.delay",
            Some("ms"),
            event_loop.lag,
        ));
        // A ratio in 0..1, which UCUM writes as the dimensionless unit `1`.
        out.extend(gauge(
            "nestjs.runtime.event_loop.utilization",
            Some("1"),
            event_loop.utilization,
        ));
    }

    if let Some(gc) = &runtime.gc {
        if let Some(count) = gc.count {
            out.push(Metric {
                name: "nestjs.runtime.gc.collections".to_string(),
                description: Some("Garbage collections in the reporting window".to_string()),
                unit: Some("{collection}"),
                kind: MetricKind::Counter,
                value: MetricValue::F64(count),
                attributes: Attributes::new(),
                timestamp: None,
            });
        }
        if let Some(duration) = gc.total_duration {
            out.push(Metric {
                name: "nestjs.runtime.gc.duration".to_string(),
                description: Some("Time spent in garbage collection in the window".to_string()),
                unit: Some("ms"),
                kind: MetricKind::Counter,
                value: MetricValue::F64(duration),
                attributes: Attributes::new(),
                timestamp: None,
            });
        }
        if let Some(breakdown) = &gc.breakdown {
            for (kind, bucket) in [
                ("minor", &breakdown.minor),
                ("major", &breakdown.major),
                ("incremental", &breakdown.incremental),
            ] {
                let Some(bucket) = bucket else { continue };
                let mut attributes = Attributes::new();
                attributes.insert(
                    "nestjs.gc.kind".to_string(),
                    Value::String(kind.to_string()),
                );

                if let Some(count) = bucket.count() {
                    out.push(Metric {
                        name: "nestjs.runtime.gc.collections.by_kind".to_string(),
                        description: None,
                        unit: Some("{collection}"),
                        kind: MetricKind::Counter,
                        value: MetricValue::F64(count),
                        attributes: attributes.clone(),
                        timestamp: None,
                    });
                }
                if let Some(duration) = bucket.duration_ms() {
                    out.push(Metric {
                        name: "nestjs.runtime.gc.duration.by_kind".to_string(),
                        description: None,
                        unit: Some("ms"),
                        kind: MetricKind::Counter,
                        value: MetricValue::F64(duration),
                        attributes,
                        timestamp: None,
                    });
                }
            }
        }
    }
}

/// Turns one application metric into one or more streams.
///
/// Every value the SDK sends for a custom metric is a map of label set to number -
/// even an unlabelled one, which arrives as `{"default": 42}` - so each field can
/// expand into several points.
///
/// A counter is exported from `increase` rather than `value`. `value` is the
/// cumulative total held in the application's own memory, which resets to zero when
/// the process restarts; `increase` is what that total rose by since the last
/// successful flush, which is exactly a delta sum and is additive across instances.
/// The SDK computes it for this reason and only advances its baseline once a flush
/// has actually been written.
///
/// A summary becomes one gauge per quantile. OTLP has a summary point type, but it
/// is legacy and thinly supported, and a `quantile` attribute is what a backend can
/// actually chart.
pub(super) fn push_custom_metric(metric: protocol::CustomMetric, out: &mut Vec<Metric>) {
    let mut base = to_attributes(metric.tags);
    // `l` is the metric's *declared* label names, not values. Only useful as
    // documentation, and it would collide with the real per-point labels below.
    let _ = &metric.labels;

    let timestamp = metric.last_updated.map(from_millis);
    let name = format!("nestjs.custom.{}", metric.name);
    let metric_type = metric.metric_type.as_deref().unwrap_or("gauge");

    // A type a later SDK introduces is still exported, as a gauge, but says what it
    // really was rather than silently claiming to be one.
    if !matches!(metric_type, "counter" | "gauge" | "summary") {
        base.insert(
            "nestjs.metric.type".to_string(),
            Value::String(metric_type.to_string()),
        );
    }

    let mut emit = |name: String,
                    kind: MetricKind,
                    source: &Option<Value>,
                    suffix_attrs: Option<(&str, &str)>| {
        let Some(source) = source else { return };
        for (labels, number) in flatten_series(source) {
            let mut attributes = base.clone();
            attributes.extend(labels);
            if let Some((key, value)) = suffix_attrs {
                attributes.insert(key.to_string(), Value::String(value.to_string()));
            }
            out.push(Metric {
                name: name.clone(),
                description: metric.description.clone(),
                unit: None,
                kind,
                value: MetricValue::F64(number),
                attributes,
                timestamp,
            });
        }
    };

    match metric_type {
        "summary" => {
            for (quantile, source) in [
                ("0.5", &metric.p50),
                ("0.95", &metric.p95),
                ("0.99", &metric.p99),
            ] {
                emit(
                    name.clone(),
                    MetricKind::Gauge,
                    source,
                    Some(("quantile", quantile)),
                );
            }
            emit(
                format!("{name}.count"),
                MetricKind::Counter,
                &metric.observations,
                None,
            );
            emit(
                format!("{name}.sum"),
                MetricKind::Counter,
                &metric.total,
                None,
            );
            emit(
                format!("{name}.max"),
                MetricKind::Gauge,
                &metric.maximum,
                None,
            );
        }
        "counter" => {
            // Prefer the delta; fall back to the cumulative reading when an older
            // SDK sent no `iv`, which is better than reporting nothing.
            let source = if metric.increase.is_some() {
                &metric.increase
            } else {
                &metric.value
            };
            emit(name.clone(), MetricKind::Counter, source, None);
        }
        _ => emit(name.clone(), MetricKind::Gauge, &metric.value, None),
    }
}

/// Reads one metric field into its individual series.
///
/// The SDK keys each series by `stringifyLabel` - `JSON.stringify` of the label
/// object with its keys sorted - and uses the literal `"default"` for a metric with
/// no labels. Both are decoded here: a JSON object key becomes real attributes, so
/// `{"route":"/login"}` arrives in the backend as a filterable `route` attribute
/// rather than as a string nobody can group by. A bare number is also accepted, in
/// case the shape is ever corrected to match the SDK's own contract.
fn flatten_series(value: &Value) -> Vec<(Attributes, f64)> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .map(|number| vec![(Attributes::new(), number)])
            .unwrap_or_default(),
        Value::Object(map) => map
            .iter()
            .filter_map(|(key, value)| Some((parse_label_key(key), value.as_f64()?)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Turns a series key back into attributes.
///
/// Anything that is not a JSON object - `"default"`, or a key from some future
/// encoding - yields no attributes rather than a made-up one. For `"default"` that
/// is exactly right: it means the metric was never labelled.
fn parse_label_key(key: &str) -> Attributes {
    if key == "default" {
        return Attributes::new();
    }
    match serde_json::from_str::<Value>(key) {
        Ok(Value::Object(map)) => map.into_iter().collect(),
        _ => Attributes::new(),
    }
}

#[cfg(test)]
mod tests;
