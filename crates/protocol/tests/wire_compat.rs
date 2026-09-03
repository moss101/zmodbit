//! Wire-compatibility tests for the canonical protocol slice (M0.3).
//!
//! `canonical_task_event` / `canonical_command_envelope` are the fixed
//! reference messages. Their encoded form is committed as a golden fixture
//! under `proto/fixtures/`; both the Rust side and the TypeScript side
//! (`packages/surface-protocol`) decode the same fixture and re-encode it
//! byte-identically, proving Rust↔TS wire compatibility in both directions.

use modbit_protocol::modbit::protocol::v1 as pb;
use prost::Message;

pub const TASK_EVENT_FIXTURE: &str = "../../proto/fixtures/task_event_v1.bin";
pub const COMMAND_FIXTURE: &str = "../../proto/fixtures/command_envelope_v1.bin";

fn schema_version() -> pb::SchemaVersion {
    pb::SchemaVersion { major: 1, minor: 0 }
}

pub fn canonical_task_event() -> pb::TaskEvent {
    pb::TaskEvent {
        task_id: "0198c7a2-7b10-7cc2-9d4e-6a1f2b3c4d5e".into(),
        generation: 7,
        payload: Some(pb::task_event::Payload::TaskCreated(pb::TaskCreated {
            session_id: "0198c7a2-7b10-7cc2-9d4e-000000000001".into(),
            title: "Implement event store projections".into(),
            prompt: "Create the durable task aggregate with idempotent commands.".into(),
            initial_status: pb::TaskStatus::Queued as i32,
        })),
    }
}

pub fn canonical_command_envelope() -> pb::CommandEnvelope {
    pb::CommandEnvelope {
        command_id: "0198c7a2-7b10-7cc2-9d4e-ffffffffffff".into(),
        tenant_id: "tenant-alpha".into(),
        user_id: "user-mohsin".into(),
        session_id: Some("0198c7a2-7b10-7cc2-9d4e-000000000001".into()),
        aggregate_id: Some("0198c7a2-7b10-7cc2-9d4e-6a1f2b3c4d5e".into()),
        expected_generation: Some(7),
        command_type: "CreateTask".into(),
        schema_version: Some(schema_version()),
        payload: Vec::new(),
        issued_at: Some(prost_types::Timestamp {
            seconds: 1_785_000_000,
            nanos: 123_000_000,
        }),
    }
}

#[test]
fn rust_encode_is_deterministic() {
    let a = canonical_task_event().encode_to_vec();
    let b = canonical_task_event().encode_to_vec();
    assert_eq!(
        a, b,
        "prost encoding must be deterministic for the same message"
    );
}

#[test]
fn rust_round_trip_task_event() {
    let original = canonical_task_event();
    let decoded = pb::TaskEvent::decode(original.encode_to_vec().as_slice()).unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn rust_round_trip_command_envelope() {
    let original = canonical_command_envelope();
    let decoded = pb::CommandEnvelope::decode(original.encode_to_vec().as_slice()).unwrap();
    assert_eq!(original, decoded);
}

/// Golden fixture: the committed bytes were produced by THIS encoder. If this
/// test fails, the Rust wire format drifted from the schema the TS side was
/// compatibility-tested against.
#[test]
fn rust_encode_matches_committed_fixture() {
    let expected = std::fs::read(TASK_EVENT_FIXTURE).expect("task event fixture present");
    assert_eq!(canonical_task_event().encode_to_vec(), expected);

    let expected = std::fs::read(COMMAND_FIXTURE).expect("command fixture present");
    assert_eq!(canonical_command_envelope().encode_to_vec(), expected);
}

/// Regenerates the golden fixtures. Run with:
///   MODBIT_WRITE_FIXTURES=1 cargo test -p modbit-protocol write_fixtures
#[test]
fn write_fixtures() {
    if std::env::var("MODBIT_WRITE_FIXTURES").is_err() {
        return;
    }
    std::fs::write(TASK_EVENT_FIXTURE, canonical_task_event().encode_to_vec()).unwrap();
    std::fs::write(
        COMMAND_FIXTURE,
        canonical_command_envelope().encode_to_vec(),
    )
    .unwrap();
}
