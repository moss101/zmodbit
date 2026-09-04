//! Real-transport integration tests for the authenticated SurfaceProtocol
//! (M1.3): actual Unix domain socket / named pipe connections, boot-secret
//! HMAC authentication, capability negotiation, and framed data exchange.

use std::sync::Arc;

use prost::Message;

use modbit_protocol::modbit::protocol::v1 as pb;
use modbit_protocol::transport::{
    bind, connect, connect_with_version, BootSecret, EndpointName, FrameHandler, TransportError,
};

fn test_endpoint() -> EndpointName {
    EndpointName::ephemeral("m1.3-test").expect("endpoint name")
}

fn serve_echo(endpoint: &EndpointName, secret: &BootSecret) {
    let listener = bind(endpoint).expect("bind listener");
    let secret = secret.clone();
    let handler: FrameHandler = Arc::new(|data| data.to_vec());
    std::thread::spawn(move || serve(listener, secret, handler));
}

use modbit_protocol::transport::serve;

fn canonical_payload() -> Vec<u8> {
    // A real protobuf message from the canonical schema rides the frame.
    pb::CommandEnvelope {
        command_id: "0198c7a2-7b10-7cc2-9d4e-ffffffffffff".into(),
        tenant_id: "tenant-alpha".into(),
        user_id: "user-mohsin".into(),
        ..Default::default()
    }
    .encode_to_vec()
}

#[test]
fn wrong_secret_is_rejected_and_server_keeps_serving() {
    let endpoint = test_endpoint();
    let secret = BootSecret::generate().expect("boot secret");
    serve_echo(&endpoint, &secret);

    // A peer without the boot secret generates a different HMAC.
    let wrong = BootSecret::generate().expect("boot secret");
    let secret = secret.clone();
    match connect(&endpoint, &wrong) {
        Err(TransportError::AuthRejected { .. }) => {}
        other => panic!("expected auth rejection, got {other:?}"),
    }

    // The server accept loop must have survived the rejection.
    let mut conn = connect(&endpoint, &secret).expect("legit client still connects");
    assert!(!conn.read_only);
    let payload = canonical_payload();
    conn.send(&payload).expect("send after rejected peer");
    assert_eq!(conn.receive().expect("echo"), payload);
}

#[test]
fn version_negotiation_flags_read_only_on_major_mismatch() {
    let endpoint = test_endpoint();
    let secret = BootSecret::generate().expect("boot secret");
    serve_echo(&endpoint, &secret);

    // A hypothetical future client: same secret, newer major.
    let conn = connect_with_version(&endpoint, &secret, 2, 3).expect("connects");
    assert!(
        conn.read_only,
        "major mismatch must flag read-only (docs/30)"
    );
    assert_eq!(
        conn.negotiated.0, 1,
        "negotiated major is the server's major"
    );
    assert_eq!(conn.negotiated.1, 0, "negotiated minor is the minimum");
    drop(conn);

    // Minor-version skew within the same major negotiates the lower minor.
    let conn = connect_with_version(&endpoint, &secret, 1, 9).expect("connects");
    assert!(!conn.read_only);
    assert_eq!(conn.negotiated.1, 0);
}

#[test]
fn framed_data_round_trips_through_real_socket() {
    let endpoint = test_endpoint();
    let secret = BootSecret::generate().expect("boot secret");
    serve_echo(&endpoint, &secret);

    let mut conn = connect(&endpoint, &secret).expect("authenticated connect");
    for i in 0..25u32 {
        let payload = pb::CommandEnvelope {
            command_id: format!("cmd-{i}"),
            tenant_id: "tenant-alpha".into(),
            user_id: "user-mohsin".into(),
            ..Default::default()
        }
        .encode_to_vec();
        conn.send(&payload).expect("send");
        assert_eq!(
            conn.receive().expect("echo"),
            payload,
            "frame {i} round trip"
        );
    }
}
