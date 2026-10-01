//! Tests for metric mapping.

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
fn counters_export_as_monotonic_delta_sums() {
    // The SDK zeroes its counters after every collection window, so they must be
    // exported with delta temporality rather than as cumulative totals, or a
    // backend reads each window's figure as a running total that keeps resetting.
    for (label, payload, name, expected) in [
        (
            "runtime gc",
            r#"{"serviceId":"api","runtime":{"g":{"c":3,"td":12.5}}}"#,
            "nestjs.runtime.gc.collections",
            3.0,
        ),
        (
            // `iv` is the increase since the last flush; `v` is the lifetime
            // total, and exporting that as a delta would wildly overcount.
            "a custom counter reports its increase, not its total",
            r#"{"serviceId":"api","custom":[
                {"n":"orders","t":"counter","v":{"default":500},"iv":{"default":12}}
            ]}"#,
            "nestjs.custom.orders",
            12.0,
        ),
    ] {
        let metrics = mapped(payload);
        let metric = find(&metrics, name);
        let metric::Data::Sum(sum) = metric.data.as_ref().unwrap() else {
            panic!("{label}: expected a sum");
        };
        assert_eq!(
            sum.aggregation_temporality,
            AggregationTemporality::Delta as i32,
            "{label}: temporality"
        );
        assert!(sum.is_monotonic, "{label}: monotonic");
        assert_eq!(double(metric), expected, "{label}: value");
    }
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
    let metrics =
        mapped(r#"{"serviceId":"api","custom":[{"n":"depth","t":"gauge","v":{"default":17}}]}"#);
    assert!(matches!(
        find(&metrics, "nestjs.custom.depth").data.as_ref().unwrap(),
        metric::Data::Gauge(_)
    ));
}

#[test]
fn a_point_is_dated_by_the_sdk_when_it_can_be_and_at_receipt_otherwise() {
    let received = at("2026-10-01T12:00:00Z").timestamp_nanos_opt().unwrap() as u64;
    for (label, payload, expected) in [
        (
            "a runtime sample carries no timestamp of its own",
            r#"{"serviceId":"api","runtime":{"m":{"hu":1.0}}}"#,
            received,
        ),
        (
            "a metric's own `lu` wins over receipt time",
            r#"{"serviceId":"api","custom":[
                {"n":"x","t":"gauge","v":{"default":1},"lu":1759312800000}
            ]}"#,
            1_759_312_800_000_000_000,
        ),
    ] {
        let metrics = mapped(payload);
        let metric::Data::Gauge(gauge) = metrics[0].data.as_ref().unwrap() else {
            panic!("{label}: expected a gauge");
        };
        assert_eq!(gauge.data_points[0].time_unix_nano, expected, "{label}");
    }
}

#[test]
fn a_batch_with_no_metrics_maps_to_nothing() {
    let batch = decoder::decode(br#"{"serviceId":"api"}"#, false).unwrap();
    assert!(map(&batch, at("2026-10-01T12:00:00Z")).is_none());
}
