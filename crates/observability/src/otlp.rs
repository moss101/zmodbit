//! OTLP/JSON export of the run-cost counters (Phase 5 item 4, docs/34 §
//! Run-cost accounting): builds an OTLP/JSON logs export request body
//! from a [`RunCostLedger`] and, when `OTEL_EXPORTER_OTLP_ENDPOINT` is
//! configured, POSTs it to `<endpoint>/v1/logs`. The payload builder is
//! pure and unit-tested; the HTTP send is feature-gated (`otel`).
//!
//! One OTLP log record per invocation entry, with the run totals as log
//! attributes — collectors that understand the logs signal can index
//! cost per task without any Modbit-specific schema.

use crate::RunCostLedger;
use serde_json::{json, Value};

/// Builds the OTLP/JSON `ExportLogsServiceRequest` body for a ledger.
/// Deterministic: same ledger in, same JSON out (attribute keys sorted by
/// serde_json's preserve-order of insertion here — the collector is
/// schema-driven, not order-driven).
pub fn otlp_logs_payload(ledger: &RunCostLedger, task_id: &str) -> Value {
    let scope_attrs = json!([
        { "key": "modbit.model", "value": { "stringValue": ledger.model } },
        { "key": "modbit.invocations", "value": { "asInt": ledger.invocations } },
        { "key": "modbit.input_tokens", "value": { "asInt": ledger.input_tokens } },
        { "key": "modbit.output_tokens", "value": { "asInt": ledger.output_tokens } },
        { "key": "modbit.cost_usd", "value": { "asDouble": ledger.cost_usd } },
        { "key": "modbit.unpriced_invocations", "value": { "asInt": ledger.unpriced_invocations } },
    ]);
    let records: Vec<Value> = ledger
        .entries()
        .iter()
        .map(|e| {
            json!({
                "timeUnixNano": 0u64,
                "severityText": "INFO",
                "body": { "stringValue": format!("modbit invocation {}", e.model) },
                "attributes": [
                    { "key": "modbit.model", "value": { "stringValue": e.model } },
                    { "key": "modbit.input_tokens", "value": { "asInt": e.input_tokens } },
                    { "key": "modbit.output_tokens", "value": { "asInt": e.output_tokens } },
                    { "key": "modbit.cost_usd", "value": { "asDouble": e.cost_usd } },
                    { "key": "modbit.priced", "value": { "boolValue": e.priced } },
                ],
            })
        })
        .collect();
    json!({
        "resourceLogs": [{
            "resource": { "attributes": [
                { "key": "service.name", "value": { "stringValue": "modbit-core" } },
                { "key": "modbit.task_id", "value": { "stringValue": task_id } },
            ]},
            "scopeLogs": [{
                "scope": { "name": "modbit-observability", "version": "0.1.0" },
                "logRecords": records,
                "attributes": scope_attrs,
            }],
        }]
    })
}

/// POSTs the payload to `<endpoint>/v1/logs` (feature `otel`).
#[cfg(feature = "otel")]
pub fn export_otlp_logs(
    endpoint: &str,
    ledger: &RunCostLedger,
    task_id: &str,
) -> Result<(), String> {
    let url = format!("{}/v1/logs", endpoint.trim_end_matches('/'));
    let body = otlp_logs_payload(ledger, task_id).to_string();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("otlp export status {}", response.status()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunCostLedger;

    /// The payload builder: deterministic, complete, and shaped as the
    /// OTLP/JSON logs signal.
    #[test]
    fn otlp_payload_shape_is_complete_and_deterministic() {
        let mut ledger = RunCostLedger::new("gpt-4o-mini");
        ledger.record(&crate::invocation_cost("gpt-4o-mini", 1_000, 500));
        ledger.record(&crate::invocation_cost("gpt-4o-mini", 2_000, 100));

        let first = otlp_logs_payload(&ledger, "task-1");
        let second = otlp_logs_payload(&ledger, "task-1");
        assert_eq!(first, second, "deterministic");

        let body = first.to_string();
        assert!(body.contains("resourceLogs"));
        assert!(body.contains("modbit-core"));
        assert!(body.contains("task-1"));
        assert!(body.contains("gpt-4o-mini"));
        // Two invocation entries exported as log records.
        let count = body.matches("\"body\"").count();
        assert_eq!(count, 2, "one log record per invocation: {body}");
        // Run totals ride the scope attributes.
        assert!(body.contains("modbit.cost_usd"));
        assert!(body.contains("modbit.unpriced_invocations"));
    }

    /// An empty ledger exports an empty record set without panicking.
    #[test]
    fn otlp_payload_handles_empty_ledger() {
        let ledger = RunCostLedger::new("m");
        let body = otlp_logs_payload(&ledger, "task-0").to_string();
        assert!(body.contains("logRecords"));
        assert!(!body.contains("\"body\""), "no records expected: {body}");
    }
}
