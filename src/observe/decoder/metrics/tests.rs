//! Tests for metric decoding.

use super::*;
use crate::observe::decoder::decode;
#[test]
fn runtime_memory_is_reported_in_megabytes() {
    let payload = br#"{"serviceId":"api","runtime":{"m":{"r":128.5,"hu":64.0}}}"#;
    let batch = decode(payload, false).unwrap();
    let rss = batch
        .metrics
        .iter()
        .find(|metric| metric.name == "nestjs.runtime.memory.rss")
        .unwrap();
    assert_eq!(rss.unit, Some("MBy"));
    assert!(matches!(rss.value, MetricValue::F64(value) if value == 128.5));
}

#[test]
fn gc_breakdown_accepts_the_object_shape_the_sdk_actually_sends() {
    // The SDK's own contract declares these as numbers; 0.3.5 sends objects.
    let payload = br#"{"serviceId":"api","runtime":{"g":{"c":3,"td":12.5,
            "b":{"m":{"count":2,"duration":4.0},"j":{"count":1,"duration":8.5}}}}}"#;
    let batch = decode(payload, false).unwrap();
    let by_kind: Vec<_> = batch
        .metrics
        .iter()
        .filter(|metric| metric.name == "nestjs.runtime.gc.collections.by_kind")
        .collect();
    assert_eq!(by_kind.len(), 2);
    let durations: Vec<_> = batch
        .metrics
        .iter()
        .filter(|metric| metric.name == "nestjs.runtime.gc.duration.by_kind")
        .collect();
    assert_eq!(durations.len(), 2);
}

#[test]
fn gc_breakdown_still_accepts_bare_numbers() {
    let payload = br#"{"serviceId":"api","runtime":{"g":{"b":{"m":2,"j":1}}}}"#;
    let batch = decode(payload, false).unwrap();
    let by_kind: Vec<_> = batch
        .metrics
        .iter()
        .filter(|metric| metric.name == "nestjs.runtime.gc.collections.by_kind")
        .collect();
    assert_eq!(by_kind.len(), 2);
}

#[test]
fn gc_counters_are_deltas_not_gauges() {
    let payload = br#"{"serviceId":"api","runtime":{"g":{"c":3,"td":12.5}}}"#;
    let batch = decode(payload, false).unwrap();
    let collections = batch
        .metrics
        .iter()
        .find(|metric| metric.name == "nestjs.runtime.gc.collections")
        .unwrap();
    assert_eq!(collections.kind, MetricKind::Counter);
}

#[test]
fn an_unlabelled_counter_yields_one_series_with_no_label_attributes() {
    // The SDK sends `{"default": n}` for a metric that was never labelled.
    let payload = br#"{"serviceId":"api","custom":[
            {"n":"orders.placed","t":"counter","v":{"default":42},
             "iv":{"default":7},"tg":{"region":"eu"}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.metrics.len(), 1);
    let metric = &batch.metrics[0];
    assert_eq!(metric.name, "nestjs.custom.orders.placed");
    assert_eq!(metric.kind, MetricKind::Counter);
    assert_eq!(metric.attributes.get("region").unwrap(), "eu");
    // `default` is not a label, so it must not become an attribute.
    assert!(!metric.attributes.contains_key("default"));
    // The delta is exported, not the cumulative total.
    assert!(matches!(metric.value, MetricValue::F64(value) if value == 7.0));
}

#[test]
fn a_labelled_counter_becomes_one_point_per_label_set() {
    let payload = br#"{"serviceId":"api","custom":[
            {"n":"logins","t":"counter",
             "iv":{"{\"route\":\"/login\"}":3,"{\"route\":\"/sso\"}":5}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.metrics.len(), 2);
    // The serialized label key is decoded back into a real attribute.
    let login = batch
        .metrics
        .iter()
        .find(|metric| metric.attributes.get("route") == Some(&Value::String("/login".into())))
        .unwrap();
    assert!(matches!(login.value, MetricValue::F64(value) if value == 3.0));
}

#[test]
fn a_counter_without_a_delta_falls_back_to_its_total() {
    let payload = br#"{"serviceId":"api","custom":[
            {"n":"orders.placed","t":"counter","v":{"default":42}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert!(matches!(batch.metrics[0].value, MetricValue::F64(value) if value == 42.0));
}

#[test]
fn a_gauge_reports_its_current_value() {
    let payload = br#"{"serviceId":"api","custom":[
            {"n":"queue.depth","t":"gauge","v":{"default":17}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.metrics[0].kind, MetricKind::Gauge);
    assert!(matches!(batch.metrics[0].value, MetricValue::F64(value) if value == 17.0));
}

#[test]
fn a_summary_becomes_one_gauge_per_quantile() {
    let payload = br#"{"serviceId":"api","custom":[
            {"n":"checkout.ms","t":"summary",
             "q50":{"default":12.0},"q95":{"default":40.0},"q99":{"default":90.0},
             "ct":{"default":100},"sm":{"default":2000}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    let quantiles: Vec<_> = batch
        .metrics
        .iter()
        .filter(|metric| metric.name == "nestjs.custom.checkout.ms")
        .collect();
    assert_eq!(quantiles.len(), 3);
    assert!(quantiles.iter().any(|metric| {
        metric.attributes.get("quantile") == Some(&Value::String("0.95".to_string()))
    }));
    assert!(
        batch
            .metrics
            .iter()
            .any(|metric| metric.name == "nestjs.custom.checkout.ms.count")
    );
}

#[test]
fn a_custom_metric_does_not_reject_the_batch_it_rides_in() {
    // Regression: typing `v` as a number made serde fail the entire batch - the
    // snapshots included - over one custom metric's per-label map.
    let payload = br#"{"serviceId":"api",
            "snapshots":[{"ti":"t1","p":"http","op":"/x","d":1}],
            "custom":[{"n":"c","t":"counter","v":{"default":1}}]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.operations.len(), 1);
    assert_eq!(batch.metrics.len(), 1);
}
