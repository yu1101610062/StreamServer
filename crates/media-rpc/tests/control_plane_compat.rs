use std::collections::BTreeMap;

use prost::Message;
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    FileDescriptorSet, field_descriptor_proto,
};

fn control_plane_file() -> FileDescriptorProto {
    FileDescriptorSet::decode(media_rpc::CONTROL_PLANE_FILE_DESCRIPTOR_SET)
        .expect("control-plane descriptor set must decode")
        .file
        .into_iter()
        .find(|file| file.name.as_deref() == Some("control_plane.proto"))
        .expect("control_plane.proto must be present")
}

fn message<'a>(file: &'a FileDescriptorProto, name: &str) -> &'a DescriptorProto {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("message {name} must exist"))
}

fn enumeration<'a>(file: &'a FileDescriptorProto, name: &str) -> &'a EnumDescriptorProto {
    file.enum_type
        .iter()
        .find(|enumeration| enumeration.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("enum {name} must exist"))
}

fn fields(message: &DescriptorProto) -> BTreeMap<&str, i32> {
    message
        .field
        .iter()
        .map(|field| {
            (
                field.name.as_deref().expect("field name"),
                field.number.expect("field number"),
            )
        })
        .collect()
}

fn field<'a>(message: &'a DescriptorProto, name: &str) -> &'a FieldDescriptorProto {
    message
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("field {name} must exist"))
}

fn enum_values(enumeration: &EnumDescriptorProto) -> BTreeMap<&str, i32> {
    enumeration
        .value
        .iter()
        .map(|value| {
            (
                value.name.as_deref().expect("enum value name"),
                value.number.expect("enum value number"),
            )
        })
        .collect()
}

fn assert_message_field(message: &DescriptorProto, name: &str, number: i32, type_name: &str) {
    let field = field(message, name);
    assert_eq!(field.number, Some(number), "{name} field number changed");
    assert_eq!(
        field.type_name.as_deref(),
        Some(type_name),
        "{name} field type changed"
    );
}

#[test]
fn envelope_and_register_changes_are_append_only() {
    let file = control_plane_file();
    let agent_envelope = message(&file, "AgentEnvelope");
    assert_eq!(
        fields(agent_envelope),
        BTreeMap::from([
            ("register", 1),
            ("heartbeat", 2),
            ("capability_snapshot", 3),
            ("task_event", 4),
            ("task_log_batch", 5),
            ("task_progress", 6),
            ("task_snapshot", 7),
            ("certificate_rotation_request", 8),
            ("certificate_rotation_activated", 9),
            ("zlm_debug_response", 10),
            ("zlm_hook_request", 11),
        ])
    );
    assert_message_field(
        agent_envelope,
        "certificate_rotation_request",
        8,
        ".streamserver.controlplane.CertificateRotationRequest",
    );
    assert_message_field(
        agent_envelope,
        "certificate_rotation_activated",
        9,
        ".streamserver.controlplane.CertificateRotationActivated",
    );
    assert_message_field(
        agent_envelope,
        "zlm_debug_response",
        10,
        ".streamserver.controlplane.ZlmDebugResponse",
    );
    assert_message_field(
        agent_envelope,
        "zlm_hook_request",
        11,
        ".streamserver.controlplane.ZlmHookRequest",
    );

    let core_envelope = message(&file, "CoreEnvelope");
    assert_eq!(
        fields(core_envelope),
        BTreeMap::from([
            ("start_task", 1),
            ("stop_task", 2),
            ("probe_capabilities", 3),
            ("adopt_orphans", 4),
            ("task_recording_control", 5),
            ("certificate_rotation_bundle", 6),
            ("activate_certificate_rotation", 7),
            ("zlm_debug_request", 8),
            ("certificate_rotation_reset", 9),
            ("zlm_hook_response", 10),
        ])
    );
    assert_message_field(
        core_envelope,
        "certificate_rotation_bundle",
        6,
        ".streamserver.controlplane.CertificateRotationBundle",
    );
    assert_message_field(
        core_envelope,
        "activate_certificate_rotation",
        7,
        ".streamserver.controlplane.ActivateCertificateRotation",
    );
    assert_message_field(
        core_envelope,
        "zlm_debug_request",
        8,
        ".streamserver.controlplane.ZlmDebugRequest",
    );
    assert_message_field(
        core_envelope,
        "certificate_rotation_reset",
        9,
        ".streamserver.controlplane.CertificateRotationReset",
    );
    assert_message_field(
        core_envelope,
        "zlm_hook_response",
        10,
        ".streamserver.controlplane.ZlmHookResponse",
    );

    let register = message(&file, "Register");
    assert_eq!(
        fields(register),
        BTreeMap::from([
            ("node_id", 1),
            ("node_name", 2),
            ("agent_version", 3),
            ("hostname", 4),
            ("labels", 5),
            ("interfaces", 6),
            ("zlm_api_base", 7),
            ("zlm_api_secret", 8),
            ("agent_stream_addr", 9),
            ("network_mode", 10),
            ("ffmpeg_bin", 11),
            ("ffprobe_bin", 12),
            ("zlm_server_id", 13),
            ("output_mount_relative_prefix_mp4", 14),
            ("output_mount_relative_prefix_hls", 15),
            ("zlm_rtmp_port", 16),
            ("zlm_rtsp_port", 17),
            ("agent_http_base_url", 18),
            ("management_port", 19),
            ("management_upload_max_bytes", 20),
        ])
    );
    assert_eq!(
        field(register, "management_port").r#type,
        Some(field_descriptor_proto::Type::Uint32 as i32)
    );
    assert_eq!(
        field(register, "management_upload_max_bytes").r#type,
        Some(field_descriptor_proto::Type::Uint64 as i32)
    );
    for deprecated in ["zlm_api_base", "zlm_api_secret", "agent_http_base_url"] {
        assert_eq!(
            field(register, deprecated)
                .options
                .as_ref()
                .and_then(|options| options.deprecated),
            Some(true),
            "Register.{deprecated} must remain present but deprecated"
        );
    }
}

#[test]
fn certificate_rotation_messages_are_idempotent_expiring_and_dual_identity() {
    let file = control_plane_file();
    assert_eq!(
        fields(message(&file, "CertificateRotationRequest")),
        BTreeMap::from([
            ("rotation_id", 1),
            ("control_csr_pem", 2),
            ("management_csr_pem", 3),
        ])
    );
    assert_eq!(
        fields(message(&file, "CertificateRotationBundle")),
        BTreeMap::from([
            ("rotation_id", 1),
            ("expires_at_ms", 2),
            ("control_certificate_pem", 3),
            ("control_fingerprint_sha256", 4),
            ("control_serial_number", 5),
            ("control_not_before_ms", 6),
            ("control_not_after_ms", 7),
            ("management_certificate_pem", 8),
            ("management_fingerprint_sha256", 9),
            ("management_serial_number", 10),
            ("management_not_before_ms", 11),
            ("management_not_after_ms", 12),
            ("agent_client_issuer_ca_pem", 13),
            ("control_plane_server_ca_pem", 14),
            ("management_client_ca_pem", 15),
            ("capability_jwt_public_key_pem", 16),
            ("capability_jwt_kid", 17),
        ])
    );
    assert_eq!(
        fields(message(&file, "ActivateCertificateRotation")),
        BTreeMap::from([("rotation_id", 1), ("previous_identity_expires_at_ms", 2),])
    );
    assert_eq!(
        fields(message(&file, "CertificateRotationActivated")),
        BTreeMap::from([
            ("rotation_id", 1),
            ("activated_at_ms", 2),
            ("control_fingerprint_sha256", 3),
            ("management_fingerprint_sha256", 4),
        ])
    );
    assert_eq!(
        enum_values(enumeration(&file, "CertificateRotationResetReason")),
        BTreeMap::from([
            ("CERTIFICATE_ROTATION_RESET_REASON_UNSPECIFIED", 0),
            ("CERTIFICATE_ROTATION_RESET_REASON_EXPIRED", 1),
        ])
    );
    assert_eq!(
        fields(message(&file, "CertificateRotationReset")),
        BTreeMap::from([("rotation_id", 1), ("reason", 2)])
    );
}

#[test]
fn zlm_debug_protocol_uses_an_operation_allowlist_and_typed_payloads() {
    let file = control_plane_file();
    assert_eq!(
        enum_values(enumeration(&file, "ZlmDebugOperation")),
        BTreeMap::from([
            ("ZLM_DEBUG_OPERATION_UNSPECIFIED", 0),
            ("ZLM_DEBUG_OPERATION_LIST_MEDIA", 1),
            ("ZLM_DEBUG_OPERATION_LIST_SESSIONS", 2),
            ("ZLM_DEBUG_OPERATION_LIST_PLAYERS", 3),
            ("ZLM_DEBUG_OPERATION_GET_STATISTIC", 4),
            ("ZLM_DEBUG_OPERATION_GET_THREADS_LOAD", 5),
            ("ZLM_DEBUG_OPERATION_GET_WORK_THREADS_LOAD", 6),
            ("ZLM_DEBUG_OPERATION_KICK_SESSION", 7),
            ("ZLM_DEBUG_OPERATION_KICK_SESSIONS", 8),
            ("ZLM_DEBUG_OPERATION_CLOSE_STREAM", 9),
            ("ZLM_DEBUG_OPERATION_SNAPSHOT", 10),
        ])
    );
    assert_eq!(
        fields(message(&file, "ZlmDebugRequest")),
        BTreeMap::from([
            ("request_id", 1),
            ("operation", 2),
            ("media_filter", 3),
            ("kick_session", 4),
            ("kick_sessions", 5),
            ("close_stream", 6),
            ("snapshot", 7),
        ])
    );
    let request = message(&file, "ZlmDebugRequest");
    assert_eq!(request.oneof_decl.len(), 1);
    assert_eq!(request.oneof_decl[0].name.as_deref(), Some("parameters"));
    for parameter in [
        "media_filter",
        "kick_session",
        "kick_sessions",
        "close_stream",
        "snapshot",
    ] {
        assert_eq!(field(request, parameter).oneof_index, Some(0));
    }
    assert!(
        request.field.iter().all(|field| {
            !matches!(
                field.name.as_deref(),
                Some("api_path" | "api_secret" | "parameters_json")
            )
        }),
        "ZLM debug must not regain arbitrary API path, secret, or parameter JSON fields"
    );

    assert_eq!(
        fields(message(&file, "ZlmMediaFilter")),
        BTreeMap::from([("schema", 1), ("vhost", 2), ("app", 3), ("stream", 4),])
    );
    assert_eq!(
        fields(message(&file, "ZlmKickSessionParameters")),
        BTreeMap::from([("session_id", 1)])
    );
    assert_eq!(
        fields(message(&file, "ZlmKickSessionsParameters")),
        BTreeMap::from([("local_port", 1), ("peer_ip", 2)])
    );
    assert_eq!(
        fields(message(&file, "ZlmCloseStreamParameters")),
        BTreeMap::from([
            ("schema", 1),
            ("vhost", 2),
            ("app", 3),
            ("stream", 4),
            ("force", 5),
        ])
    );
    assert_eq!(
        fields(message(&file, "ZlmSnapshotParameters")),
        BTreeMap::from([("source_url", 1), ("timeout_sec", 2), ("expire_sec", 3),])
    );

    assert_eq!(
        enum_values(enumeration(&file, "ZlmDebugResponseStatus")),
        BTreeMap::from([
            ("ZLM_DEBUG_RESPONSE_STATUS_UNSPECIFIED", 0),
            ("ZLM_DEBUG_RESPONSE_STATUS_SUCCEEDED", 1),
            ("ZLM_DEBUG_RESPONSE_STATUS_FAILED", 2),
        ])
    );
    let response = message(&file, "ZlmDebugResponse");
    assert_eq!(
        fields(response),
        BTreeMap::from([
            ("request_id", 1),
            ("operation", 2),
            ("status", 3),
            ("json_payload", 4),
            ("snapshot", 5),
            ("error", 6),
            ("truncated", 7),
        ])
    );
    assert_eq!(response.oneof_decl.len(), 1);
    assert_eq!(response.oneof_decl[0].name.as_deref(), Some("payload"));
    for payload in ["json_payload", "snapshot", "error"] {
        assert_eq!(field(response, payload).oneof_index, Some(0));
    }
    assert_eq!(
        fields(message(&file, "ZlmSnapshotPayload")),
        BTreeMap::from([("content_type", 1), ("data", 2)])
    );
    assert_eq!(
        fields(message(&file, "ZlmDebugError")),
        BTreeMap::from([("code", 1), ("message", 2)])
    );
}

#[test]
fn zlm_hook_protocol_is_session_bound_and_carries_no_remote_secret_or_server_identity() {
    let file = control_plane_file();
    let request = message(&file, "ZlmHookRequest");
    assert_eq!(
        fields(request),
        BTreeMap::from([("request_id", 1), ("hook_name", 2), ("body_json", 3),])
    );
    let response = message(&file, "ZlmHookResponse");
    assert_eq!(
        fields(response),
        BTreeMap::from([("request_id", 1), ("http_status", 2), ("body_json", 3),])
    );
    for schema in [request, response] {
        assert!(
            schema.field.iter().all(|field| {
                !matches!(
                    field.name.as_deref(),
                    Some("server_id" | "media_server_id" | "secret" | "hook_secret")
                )
            }),
            "ZLM hook forwarding must derive server identity from mTLS session and keep secrets local"
        );
    }
}
