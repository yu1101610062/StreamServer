alter table auth_users
  add column bootstrap_handoff_id uuid,
  add column bootstrap_handoff_version bigint not null default 0,
  add column bootstrap_handoff_completed_at timestamptz,
  add column credential_version bigint not null default 0;

alter table auth_users
  add constraint auth_users_bootstrap_handoff_state_check
  check (
    (bootstrap_handoff_id is null
      and bootstrap_handoff_version = 0
      and bootstrap_handoff_completed_at is null)
    or
    (bootstrap_handoff_id is not null
      and bootstrap_handoff_version > 0)
  );

alter table auth_users
  add constraint auth_users_credential_version_check
  check (credential_version >= 0);

create unique index auth_users_bootstrap_handoff_id_uidx
  on auth_users (bootstrap_handoff_id)
  where bootstrap_handoff_id is not null;

create table agent_identities (
  node_id uuid primary key,
  status text not null
    check (status in ('pending_enrollment', 'active', 'revoked')),
  created_at timestamptz not null,
  updated_at timestamptz not null,
  revoked_at timestamptz,
  revocation_reason text,
  check (
    (status = 'revoked' and revoked_at is not null)
    or (status <> 'revoked' and revoked_at is null and revocation_reason is null)
  )
);

-- Existing registered nodes have no certificate identity yet.  They are kept
-- addressable but must complete enrollment before the control plane accepts
-- them after this migration.
insert into agent_identities (node_id, status, created_at, updated_at)
select id, 'pending_enrollment', created_at, now()
  from media_nodes;

-- A pre-migration connection cannot satisfy the new certificate identity
-- requirements.  Keep last-seen timestamps as history, but fail closed on the
-- two durable online-state flags until the node enrolls and reconnects.
update media_nodes
   set healthy = false,
       control_connected = false,
       updated_at = now()
 where healthy or control_connected;

alter table media_nodes
  add constraint media_nodes_agent_identity_fk
  foreign key (id) references agent_identities(node_id) not valid;

alter table media_nodes validate constraint media_nodes_agent_identity_fk;

create table agent_enrollment_tokens (
  id uuid primary key,
  node_id uuid not null references agent_identities(node_id),
  token_hash bytea not null unique
    check (octet_length(token_hash) = 32),
  created_by text not null,
  created_at timestamptz not null,
  expires_at timestamptz not null,
  consumed_at timestamptz,
  revoked_at timestamptz,
  consumed_certificate_id uuid,
  consumed_management_certificate_id uuid,
  control_csr_public_key_sha256 bytea
    check (control_csr_public_key_sha256 is null
      or octet_length(control_csr_public_key_sha256) = 32),
  management_csr_public_key_sha256 bytea
    check (management_csr_public_key_sha256 is null
      or octet_length(management_csr_public_key_sha256) = 32),
  agent_client_issuer_ca_pem text,
  control_plane_server_ca_pem text,
  management_client_ca_pem text,
  capability_jwt_public_key_pem text,
  capability_jwt_kid text,
  check (expires_at > created_at),
  check (expires_at <= created_at + interval '10 minutes'),
  check (consumed_at is null or consumed_at >= created_at),
  check (revoked_at is null or revoked_at >= created_at),
  check (not (consumed_at is not null and revoked_at is not null)),
  check (
    (consumed_at is null
      and consumed_certificate_id is null
      and consumed_management_certificate_id is null
      and control_csr_public_key_sha256 is null
      and management_csr_public_key_sha256 is null
      and agent_client_issuer_ca_pem is null
      and control_plane_server_ca_pem is null
      and management_client_ca_pem is null
      and capability_jwt_public_key_pem is null
      and capability_jwt_kid is null)
    or
    (consumed_at is not null
      and consumed_certificate_id is not null
      and consumed_management_certificate_id is not null
      and control_csr_public_key_sha256 is not null
      and management_csr_public_key_sha256 is not null
      and agent_client_issuer_ca_pem is not null
      and control_plane_server_ca_pem is not null
      and management_client_ca_pem is not null
      and capability_jwt_public_key_pem is not null
      and capability_jwt_kid is not null
      and capability_jwt_kid <> '')
  )
);

create unique index agent_enrollment_tokens_live_node_uidx
  on agent_enrollment_tokens (node_id)
  where consumed_at is null and revoked_at is null;

create index agent_enrollment_tokens_node_created_idx
  on agent_enrollment_tokens (node_id, created_at desc);

create index agent_enrollment_tokens_expiry_idx
  on agent_enrollment_tokens (expires_at)
  where consumed_at is null and revoked_at is null;

create table agent_certificates (
  id uuid primary key,
  node_id uuid not null references agent_identities(node_id),
  serial_number text not null unique,
  fingerprint_sha256 bytea not null unique
    check (octet_length(fingerprint_sha256) = 32),
  public_key_sha256 bytea not null
    check (octet_length(public_key_sha256) = 32),
  certificate_pem text not null,
  state text not null
    check (state in ('pending_rotation', 'active', 'replaced', 'revoked')),
  not_before timestamptz not null,
  not_after timestamptz not null,
  issued_at timestamptz not null,
  activated_at timestamptz,
  revoked_at timestamptz,
  revocation_reason text,
  issued_via text not null
    check (issued_via in ('enrollment', 'rotation')),
  check (not_after > not_before),
  check (not_after <= not_before + interval '90 days'),
  check (activated_at is null or activated_at >= issued_at),
  check (revoked_at is null or revoked_at >= issued_at)
);

create unique index agent_certificates_active_node_uidx
  on agent_certificates (node_id)
  where state = 'active';

create unique index agent_certificates_pending_rotation_node_uidx
  on agent_certificates (node_id)
  where state = 'pending_rotation';

create index agent_certificates_node_fingerprint_idx
  on agent_certificates (node_id, fingerprint_sha256);

create unique index agent_certificates_id_node_uidx
  on agent_certificates (id, node_id);

create index agent_certificates_live_expiry_idx
  on agent_certificates (not_after)
  where state in ('active', 'pending_rotation');

create table agent_management_certificates (
  id uuid primary key,
  node_id uuid not null references agent_identities(node_id),
  serial_number text not null unique,
  fingerprint_sha256 bytea not null unique
    check (octet_length(fingerprint_sha256) = 32),
  public_key_sha256 bytea not null
    check (octet_length(public_key_sha256) = 32),
  certificate_pem text not null,
  state text not null
    check (state in ('pending_rotation', 'active', 'replaced', 'revoked')),
  not_before timestamptz not null,
  not_after timestamptz not null,
  issued_at timestamptz not null,
  activated_at timestamptz,
  revoked_at timestamptz,
  revocation_reason text,
  issued_via text not null
    check (issued_via in ('enrollment', 'rotation')),
  check (not_after > not_before),
  check (not_after <= not_before + interval '90 days'),
  check (activated_at is null or activated_at >= issued_at),
  check (revoked_at is null or revoked_at >= issued_at)
);

create unique index agent_management_certificates_active_node_uidx
  on agent_management_certificates (node_id)
  where state = 'active';

create unique index agent_management_certificates_pending_rotation_node_uidx
  on agent_management_certificates (node_id)
  where state = 'pending_rotation';

create index agent_management_certificates_node_fingerprint_idx
  on agent_management_certificates (node_id, fingerprint_sha256);

create unique index agent_management_certificates_id_node_uidx
  on agent_management_certificates (id, node_id);

create index agent_management_certificates_live_expiry_idx
  on agent_management_certificates (not_after)
  where state in ('active', 'pending_rotation');

alter table agent_enrollment_tokens
  add constraint agent_enrollment_tokens_consumed_certificate_fk
  foreign key (consumed_certificate_id, node_id)
  references agent_certificates(id, node_id);

alter table agent_enrollment_tokens
  add constraint agent_enrollment_tokens_consumed_management_certificate_fk
  foreign key (consumed_management_certificate_id, node_id)
  references agent_management_certificates(id, node_id);

create table agent_certificate_rotations (
  id uuid primary key,
  node_id uuid not null references agent_identities(node_id),
  old_certificate_id uuid not null,
  new_certificate_id uuid not null unique,
  old_management_certificate_id uuid not null,
  new_management_certificate_id uuid not null unique,
  control_csr_public_key_sha256 bytea not null
    check (octet_length(control_csr_public_key_sha256) = 32),
  management_csr_public_key_sha256 bytea not null
    check (octet_length(management_csr_public_key_sha256) = 32),
  state text not null default 'pending'
    check (state in (
      'pending', 'control_activated', 'management_activated',
      'completed', 'expired', 'rejected'
    )),
  authorized_at timestamptz not null,
  authorized_until timestamptz not null,
  consumed_at timestamptz,
  consumed_by_session_id uuid,
  management_activated_at timestamptz,
  management_activated_by_session_id uuid,
  completed_at timestamptz,
  completed_by_session_id uuid,
  foreign key (old_certificate_id, node_id)
    references agent_certificates(id, node_id),
  foreign key (new_certificate_id, node_id)
    references agent_certificates(id, node_id),
  foreign key (old_management_certificate_id, node_id)
    references agent_management_certificates(id, node_id),
  foreign key (new_management_certificate_id, node_id)
    references agent_management_certificates(id, node_id),
  unique (old_certificate_id, new_certificate_id),
  unique (old_management_certificate_id, new_management_certificate_id),
  check (authorized_until > authorized_at),
  check (authorized_until <= authorized_at + interval '5 minutes'),
  check (
    (consumed_at is null and consumed_by_session_id is null)
    or (consumed_at is not null and consumed_by_session_id is not null)
  ),
  check (
    (management_activated_at is null and management_activated_by_session_id is null)
    or (management_activated_at is not null and management_activated_by_session_id is not null)
  ),
  check (
    (completed_at is null and completed_by_session_id is null)
    or (completed_at is not null and completed_by_session_id is not null)
  )
);

create unique index agent_certificate_rotations_pending_node_uidx
  on agent_certificate_rotations (node_id)
  where state in ('pending', 'control_activated', 'management_activated');

create index agent_certificate_rotations_live_idx
  on agent_certificate_rotations (node_id, authorized_until)
  where consumed_at is null;

create table agent_control_sessions (
  node_id uuid primary key references agent_identities(node_id),
  session_id uuid not null unique,
  core_instance_id uuid not null,
  certificate_id uuid not null,
  peer_ip inet,
  connected_at timestamptz not null,
  last_activity_at timestamptz not null,
  lease_expires_at timestamptz not null,
  disconnected_at timestamptz,
  takeover_from_session_id uuid,
  takeover_reason text,
  foreign key (certificate_id, node_id)
    references agent_certificates(id, node_id),
  check (last_activity_at >= connected_at),
  check (lease_expires_at > connected_at),
  check (
    (takeover_from_session_id is null and takeover_reason is null)
    or (takeover_from_session_id is not null
      and takeover_reason in ('stale_timeout', 'clean_disconnect', 'certificate_rotation'))
  )
);

create index agent_control_sessions_lease_expiry_idx
  on agent_control_sessions (lease_expires_at);

create index security_audit_events_agent_identity_idx
  on security_audit_events (event_type, created_at desc)
  where event_type like 'agent_%';
