//! CloudTrail-like synthetic audit log generator.
//!
//! The goal is to mimic the nested, partially sparse, high-cardinality shape
//! of real AWS CloudTrail records closely enough that compression, hashing,
//! and downstream parsing behave realistically in benchmarks.

use super::{
    CLOUDTRAIL_EVENT_VERSION, CLOUDTRAIL_PUBLIC_IPS, CLOUDTRAIL_REGIONS_GLOBAL,
    CLOUDTRAIL_REGIONS_MULTI, CLOUDTRAIL_REGIONS_REGIONAL, CLOUDTRAIL_USER_AGENTS,
    CloudTrailActionSpec, CloudTrailIdentityKind, CloudTrailProfile, CloudTrailRegionMix,
    CloudTrailServiceKind, CloudTrailServiceMix, CloudTrailServiceSpec, SERVICE_BALANCED,
    SERVICE_COMPUTE_HEAVY, SERVICE_SECURITY_HEAVY, SERVICE_STORAGE_HEAVY, append_uuid_like,
    json_escape, pick, weighted_choice_pick, weighted_pick,
};

/// Generate CloudTrail-like audit logs using the benchmark-default profile.
pub fn gen_cloudtrail_audit(count: usize, seed: u64) -> Vec<u8> {
    gen_cloudtrail_audit_with_profile(count, seed, CloudTrailProfile::benchmark_default())
}

/// Generate CloudTrail-like audit logs using a tunable realism profile.
pub fn gen_cloudtrail_audit_with_profile(
    count: usize,
    seed: u64,
    profile: CloudTrailProfile,
) -> Vec<u8> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let state = CloudTrailState::new(profile);
    let services = cloudtrail_services(profile.service_mix);
    let mut buf = String::with_capacity(count.saturating_mul(1_400));

    for i in 0..count {
        let service = weighted_choice_pick(&mut rng, services);
        let action = pick_cloudtrail_action(&mut rng, service.actions);
        let account_slot =
            pick_tenure_slot(&mut rng, i, profile.account_tenure, state.accounts.len());
        let principal_slot = pick_tenure_slot(
            &mut rng,
            i + account_slot,
            profile.principal_tenure,
            state.principals.len(),
        );
        let account_id = state.account_id(account_slot);
        let principal_name = state.principal_name(principal_slot);
        let role_name = state.role_name(principal_slot);
        let session_name = state.session_name(principal_slot);
        let region = pick_cloudtrail_region(&mut rng, profile.region_mix, service.kind);
        let event_time = cloudtrail_event_time(i, &mut rng);
        let event_id = cloudtrail_uuid_like(&mut rng);
        let shared_event_id =
            if action.data_event || chance(&mut rng, profile.optional_field_density / 3) {
                Some(cloudtrail_uuid_like(&mut rng))
            } else {
                None
            };
        let source_ip =
            cloudtrail_source_ip(&mut rng, service.kind, profile.optional_field_density);
        let user_agent = cloudtrail_user_agent(&mut rng, service.kind, principal_slot);
        let identity_kind = pick_cloudtrail_identity_kind(&mut rng, service.kind, action.read_only);
        let user_identity = build_user_identity_json(
            &mut rng,
            identity_kind,
            account_id,
            principal_name,
            role_name,
            session_name,
            principal_slot,
            region,
            profile.optional_field_density,
            service.kind,
        );
        let request_parameters = build_request_parameters_json(
            &mut rng,
            service.kind,
            action,
            account_id,
            principal_name,
            role_name,
            session_name,
            region,
            i,
            profile.optional_field_density,
        );
        let response_elements = build_response_elements_json(
            &mut rng,
            service.kind,
            action,
            account_id,
            principal_name,
            role_name,
            session_name,
            region,
            i,
            profile.optional_field_density,
        );
        let resources = build_resources_json(
            &mut rng,
            service.kind,
            action,
            account_id,
            principal_name,
            role_name,
            session_name,
            region,
            i,
            profile.optional_field_density,
        );
        let additional_event_data = build_additional_event_data_json(
            &mut rng,
            service.kind,
            action,
            region,
            profile.optional_field_density,
        );
        let error = cloudtrail_error_pair(&mut rng, action.read_only, action.data_event);
        let event_type = cloudtrail_event_type(service.kind, identity_kind, action);
        let event_category = if action.data_event {
            "Data"
        } else {
            "Management"
        };
        let vpc_endpoint_id = if chance(&mut rng, profile.optional_field_density / 4)
            && !matches!(service.kind, CloudTrailServiceKind::CloudTrail)
        {
            Some(format!("vpce-{:08x}", rng.u32(..)))
        } else {
            None
        };
        let tls_details =
            build_tls_details_json(&mut rng, service.kind, profile.optional_field_density);

        let mut event = JsonObjectWriter::new(1_280);
        event.field_str("eventVersion", CLOUDTRAIL_EVENT_VERSION);
        event.field_str("eventTime", &event_time);
        event.field_str("eventSource", service.event_source);
        event.field_str("eventName", action.event_name);
        event.field_str("awsRegion", region);
        event.field_str("sourceIPAddress", &source_ip);
        event.field_str("userAgent", &user_agent);
        event.field_str("eventID", &event_id);
        event.field_str("eventType", event_type);
        event.field_str("recipientAccountId", account_id);
        event.field_bool("readOnly", action.read_only);
        event.field_bool("managementEvent", !action.data_event);
        event.field_str("eventCategory", event_category);
        event.field_raw("userIdentity", &user_identity);
        if let Some(request_parameters) = request_parameters {
            event.field_raw("requestParameters", &request_parameters);
        }
        if let Some(response_elements) = response_elements {
            event.field_raw("responseElements", &response_elements);
        }
        if let Some(resources) = resources {
            event.field_raw("resources", &resources);
        }
        if let Some(additional_event_data) = additional_event_data {
            event.field_raw("additionalEventData", &additional_event_data);
        }
        if let Some(shared_event_id) = shared_event_id {
            event.field_str("sharedEventID", &shared_event_id);
        }
        if let Some(vpc_endpoint_id) = vpc_endpoint_id {
            event.field_str("vpcEndpointId", &vpc_endpoint_id);
        }
        if let Some((error_code, error_message)) = error {
            event.field_str("errorCode", error_code);
            event.field_str("errorMessage", error_message);
        }
        if let Some(tls_details) = tls_details {
            event.field_raw("tlsDetails", &tls_details);
        }

        buf.push_str(&event.finish());
        buf.push('\n');
    }

    buf.into_bytes()
}

#[derive(Debug, Clone)]
struct CloudTrailState {
    accounts: Vec<String>,
    principals: Vec<String>,
    roles: Vec<String>,
    sessions: Vec<String>,
}

impl CloudTrailState {
    fn new(profile: CloudTrailProfile) -> Self {
        let mut accounts = Vec::with_capacity(profile.account_count.max(1));
        for idx in 0..profile.account_count.max(1) {
            accounts.push(format!("{:012}", 100_000_000_000u64 + (idx as u64 * 137)));
        }

        let mut principals = Vec::with_capacity(profile.principal_count.max(1));
        let mut roles = Vec::with_capacity(profile.principal_count.max(1));
        let mut sessions = Vec::with_capacity(profile.principal_count.max(1));
        for idx in 0..profile.principal_count.max(1) {
            principals.push(format!("user-{:03}", idx));
            roles.push(format!("cloudtrail-role-{:03}", idx));
            sessions.push(format!("session-{:03}", idx));
        }

        Self {
            accounts,
            principals,
            roles,
            sessions,
        }
    }

    fn account_id(&self, idx: usize) -> &str {
        &self.accounts[idx % self.accounts.len()]
    }

    fn principal_name(&self, idx: usize) -> &str {
        &self.principals[idx % self.principals.len()]
    }

    fn role_name(&self, idx: usize) -> &str {
        &self.roles[idx % self.roles.len()]
    }

    fn session_name(&self, idx: usize) -> &str {
        &self.sessions[idx % self.sessions.len()]
    }
}

struct JsonObjectWriter {
    out: String,
    first: bool,
}

impl JsonObjectWriter {
    fn new(capacity: usize) -> Self {
        let mut out = String::with_capacity(capacity);
        out.push('{');
        Self { out, first: true }
    }

    fn field_prefix(&mut self) {
        if self.first {
            self.first = false;
        } else {
            self.out.push(',');
        }
    }

    fn field_str(&mut self, key: &str, value: &str) {
        self.field_prefix();
        self.out.push('"');
        self.out.push_str(key);
        self.out.push_str("\":\"");
        json_escape(value, &mut self.out);
        self.out.push('"');
    }

    fn field_bool(&mut self, key: &str, value: bool) {
        self.field_prefix();
        self.out.push('"');
        self.out.push_str(key);
        self.out.push_str("\":");
        self.out.push_str(if value { "true" } else { "false" });
    }

    fn field_raw(&mut self, key: &str, value: &str) {
        self.field_prefix();
        self.out.push('"');
        self.out.push_str(key);
        self.out.push_str("\":");
        self.out.push_str(value);
    }

    fn finish(mut self) -> String {
        self.out.push('}');
        self.out
    }
}

fn cloudtrail_services(mix: CloudTrailServiceMix) -> &'static [CloudTrailServiceSpec] {
    match mix {
        CloudTrailServiceMix::Balanced => SERVICE_BALANCED,
        CloudTrailServiceMix::SecurityHeavy => SERVICE_SECURITY_HEAVY,
        CloudTrailServiceMix::StorageHeavy => SERVICE_STORAGE_HEAVY,
        CloudTrailServiceMix::ComputeHeavy => SERVICE_COMPUTE_HEAVY,
    }
}

fn pick_cloudtrail_action<'a>(
    rng: &mut fastrand::Rng,
    actions: &'a [CloudTrailActionSpec],
) -> &'a CloudTrailActionSpec {
    debug_assert!(!actions.is_empty());
    &actions[rng.usize(..actions.len())]
}

fn cloudtrail_region_pool(region_mix: CloudTrailRegionMix) -> &'static [&'static str] {
    match region_mix {
        CloudTrailRegionMix::GlobalOnly => CLOUDTRAIL_REGIONS_GLOBAL,
        CloudTrailRegionMix::Regional => CLOUDTRAIL_REGIONS_REGIONAL,
        CloudTrailRegionMix::MultiRegion => CLOUDTRAIL_REGIONS_MULTI,
    }
}

fn pick_cloudtrail_region(
    rng: &mut fastrand::Rng,
    region_mix: CloudTrailRegionMix,
    service_kind: CloudTrailServiceKind,
) -> &'static str {
    if matches!(
        service_kind,
        CloudTrailServiceKind::Iam | CloudTrailServiceKind::Sts | CloudTrailServiceKind::CloudTrail
    ) {
        return CLOUDTRAIL_REGIONS_GLOBAL[0];
    }
    pick(rng, cloudtrail_region_pool(region_mix))
}

fn pick_tenure_slot(
    rng: &mut fastrand::Rng,
    index: usize,
    tenure: usize,
    pool_len: usize,
) -> usize {
    if pool_len <= 1 {
        return 0;
    }
    let tenure = tenure.max(1);
    let base = index / tenure;
    let jitter = rng.usize(..2);
    (base + jitter) % pool_len
}

fn chance(rng: &mut fastrand::Rng, pct: u8) -> bool {
    pct > 0 && rng.u8(..100) < pct
}

fn cloudtrail_uuid_like(rng: &mut fastrand::Rng) -> String {
    let mut out = String::with_capacity(36);
    append_uuid_like(rng, &mut out);
    out
}

fn cloudtrail_event_time(index: usize, rng: &mut fastrand::Rng) -> String {
    let sec = index % 60;
    let nano = rng.u32(..1_000_000_000);
    format!("2024-01-15T10:30:{sec:02}.{nano:09}Z")
}

fn cloudtrail_source_ip(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    optional_density: u8,
) -> String {
    if matches!(service_kind, CloudTrailServiceKind::CloudTrail)
        && chance(rng, optional_density / 2)
    {
        return "AWS Internal".to_string();
    }
    pick(rng, CLOUDTRAIL_PUBLIC_IPS).to_string()
}

fn cloudtrail_user_agent(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    principal_slot: usize,
) -> String {
    match service_kind {
        CloudTrailServiceKind::CloudTrail => "cloudtrail.amazonaws.com".to_string(),
        CloudTrailServiceKind::Iam | CloudTrailServiceKind::Sts => {
            if principal_slot % 4 == 0 {
                "console.amazonaws.com".to_string()
            } else {
                pick(rng, CLOUDTRAIL_USER_AGENTS).to_string()
            }
        }
        CloudTrailServiceKind::S3 | CloudTrailServiceKind::Kms => {
            pick(rng, CLOUDTRAIL_USER_AGENTS).to_string()
        }
        CloudTrailServiceKind::Ec2 | CloudTrailServiceKind::Lambda | CloudTrailServiceKind::Rds => {
            if principal_slot % 3 == 0 {
                "aws-cli/2.15.0 Python/3.11.8 Linux/6.6".to_string()
            } else {
                pick(rng, CLOUDTRAIL_USER_AGENTS).to_string()
            }
        }
    }
}

fn pick_cloudtrail_identity_kind(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    read_only: bool,
) -> CloudTrailIdentityKind {
    match service_kind {
        CloudTrailServiceKind::CloudTrail => CloudTrailIdentityKind::AwsService,
        CloudTrailServiceKind::Sts => weighted_pick(
            rng,
            &[
                (CloudTrailIdentityKind::AssumedRole, 44),
                (CloudTrailIdentityKind::FederatedUser, 26),
                (CloudTrailIdentityKind::IamUser, 20),
                (CloudTrailIdentityKind::Root, 10),
            ],
        ),
        CloudTrailServiceKind::Iam => weighted_pick(
            rng,
            &[
                (CloudTrailIdentityKind::IamUser, 40),
                (CloudTrailIdentityKind::AssumedRole, 32),
                (CloudTrailIdentityKind::Root, 18),
                (CloudTrailIdentityKind::FederatedUser, 10),
            ],
        ),
        CloudTrailServiceKind::Ec2 | CloudTrailServiceKind::Rds => weighted_pick(
            rng,
            &[
                (
                    CloudTrailIdentityKind::AssumedRole,
                    if read_only { 48 } else { 58 },
                ),
                (CloudTrailIdentityKind::IamUser, 24),
                (CloudTrailIdentityKind::FederatedUser, 10),
                (CloudTrailIdentityKind::AwsService, 8),
                (CloudTrailIdentityKind::Root, 4),
            ],
        ),
        CloudTrailServiceKind::S3 => weighted_pick(
            rng,
            &[
                (
                    CloudTrailIdentityKind::AssumedRole,
                    if read_only { 52 } else { 42 },
                ),
                (CloudTrailIdentityKind::IamUser, 24),
                (CloudTrailIdentityKind::AwsService, 16),
                (CloudTrailIdentityKind::FederatedUser, 8),
            ],
        ),
        CloudTrailServiceKind::Kms => weighted_pick(
            rng,
            &[
                (CloudTrailIdentityKind::AssumedRole, 50),
                (CloudTrailIdentityKind::IamUser, 24),
                (CloudTrailIdentityKind::AwsService, 16),
                (CloudTrailIdentityKind::Root, 10),
            ],
        ),
        CloudTrailServiceKind::Lambda => weighted_pick(
            rng,
            &[
                (CloudTrailIdentityKind::AwsService, 34),
                (CloudTrailIdentityKind::AssumedRole, 40),
                (CloudTrailIdentityKind::IamUser, 18),
                (CloudTrailIdentityKind::FederatedUser, 8),
            ],
        ),
    }
}

fn cloudtrail_identity_type(identity_kind: CloudTrailIdentityKind) -> &'static str {
    match identity_kind {
        CloudTrailIdentityKind::Root => "Root",
        CloudTrailIdentityKind::IamUser => "IAMUser",
        CloudTrailIdentityKind::AssumedRole => "AssumedRole",
        CloudTrailIdentityKind::AwsService => "AWSService",
        CloudTrailIdentityKind::FederatedUser => "FederatedUser",
    }
}

fn cloudtrail_principal_id(
    identity_kind: CloudTrailIdentityKind,
    account_id: &str,
    slot: usize,
) -> String {
    match identity_kind {
        CloudTrailIdentityKind::Root => account_id.to_string(),
        CloudTrailIdentityKind::IamUser => format!("AIDA{:08X}", 0x1000_0000 + slot as u32),
        CloudTrailIdentityKind::AssumedRole => {
            format!("AROA{:08X}:{}", 0x2000_0000 + slot as u32, slot % 1_000)
        }
        CloudTrailIdentityKind::AwsService => "cloudtrail.amazonaws.com".to_string(),
        CloudTrailIdentityKind::FederatedUser => {
            format!("AROAFED{:08X}:{}", 0x3000_0000 + slot as u32, slot % 1_000)
        }
    }
}

fn cloudtrail_arn(
    identity_kind: CloudTrailIdentityKind,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
) -> String {
    match identity_kind {
        CloudTrailIdentityKind::Root => format!("arn:aws:iam::{account_id}:root"),
        CloudTrailIdentityKind::IamUser => {
            format!("arn:aws:iam::{account_id}:user/{principal_name}")
        }
        CloudTrailIdentityKind::AssumedRole => {
            format!("arn:aws:sts::{account_id}:assumed-role/{role_name}/{session_name}")
        }
        CloudTrailIdentityKind::AwsService => {
            "arn:aws:iam::aws:policy/service-role/AWSCloudTrailFullAccess".to_string()
        }
        CloudTrailIdentityKind::FederatedUser => {
            format!("arn:aws:sts::{account_id}:federated-user/{principal_name}")
        }
    }
}

fn cloudtrail_event_type(
    service_kind: CloudTrailServiceKind,
    identity_kind: CloudTrailIdentityKind,
    action: &CloudTrailActionSpec,
) -> &'static str {
    match (
        service_kind,
        identity_kind,
        action.read_only,
        action.data_event,
    ) {
        (CloudTrailServiceKind::CloudTrail, _, _, _) => "AwsServiceEvent",
        (_, CloudTrailIdentityKind::AwsService, _, _) => "AwsServiceEvent",
        (_, _, _, true) => "AwsApiCall",
        (_, CloudTrailIdentityKind::Root, _, _) => "AwsConsoleAction",
        (_, CloudTrailIdentityKind::IamUser, true, _) => "AwsConsoleAction",
        _ => "AwsApiCall",
    }
}

fn build_user_identity_json(
    rng: &mut fastrand::Rng,
    identity_kind: CloudTrailIdentityKind,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    principal_slot: usize,
    region: &str,
    optional_density: u8,
    service_kind: CloudTrailServiceKind,
) -> String {
    let principal_id = cloudtrail_principal_id(identity_kind, account_id, principal_slot);
    let arn = cloudtrail_arn(
        identity_kind,
        account_id,
        principal_name,
        role_name,
        session_name,
    );
    let mut out = JsonObjectWriter::new(384);
    out.field_str("type", cloudtrail_identity_type(identity_kind));
    out.field_str("principalId", &principal_id);
    out.field_str("accountId", account_id);
    if !matches!(identity_kind, CloudTrailIdentityKind::AwsService) {
        out.field_str("arn", &arn);
    } else if chance(rng, optional_density / 2) {
        out.field_str("invokedBy", "cloudtrail.amazonaws.com");
    }

    match identity_kind {
        CloudTrailIdentityKind::Root => {}
        CloudTrailIdentityKind::IamUser => {
            out.field_str("userName", principal_name);
            if chance(rng, optional_density) {
                out.field_str("accessKeyId", &format!("AKIA{:08X}", rng.u32(..)));
            }
        }
        CloudTrailIdentityKind::AssumedRole | CloudTrailIdentityKind::FederatedUser => {
            out.field_str("userName", principal_name);
            if chance(rng, optional_density) {
                out.field_raw(
                    "sessionContext",
                    &build_session_context_json(
                        rng,
                        account_id,
                        principal_name,
                        role_name,
                        session_name,
                        principal_slot,
                        identity_kind,
                        optional_density,
                    ),
                );
            }
        }
        CloudTrailIdentityKind::AwsService => {}
    }

    if matches!(
        service_kind,
        CloudTrailServiceKind::Lambda | CloudTrailServiceKind::Kms
    ) && chance(rng, optional_density / 2)
    {
        out.field_str("sourceIdentity", principal_name);
    }

    if matches!(
        service_kind,
        CloudTrailServiceKind::Sts | CloudTrailServiceKind::CloudTrail
    ) {
        out.field_str("invokedBy", "cloudtrail.amazonaws.com");
    } else if matches!(identity_kind, CloudTrailIdentityKind::AwsService) {
        out.field_str("invokedBy", "cloudtrail.amazonaws.com");
    }

    let _ = region;
    out.finish()
}

fn build_session_context_json(
    rng: &mut fastrand::Rng,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    principal_slot: usize,
    identity_kind: CloudTrailIdentityKind,
    optional_density: u8,
) -> String {
    let mut out = JsonObjectWriter::new(320);
    out.field_raw(
        "sessionIssuer",
        &build_session_issuer_json(
            identity_kind,
            account_id,
            principal_name,
            role_name,
            session_name,
            principal_slot,
        ),
    );
    let mut attr = JsonObjectWriter::new(160);
    attr.field_str("creationDate", &cloudtrail_event_time(rng.usize(..60), rng));
    attr.field_str(
        "mfaAuthenticated",
        if chance(rng, optional_density) {
            "true"
        } else {
            "false"
        },
    );
    if chance(rng, optional_density / 2) {
        attr.field_str("sessionCredentialFromConsole", "true");
    }
    out.field_raw("attributes", &attr.finish());
    out.finish()
}

fn build_session_issuer_json(
    identity_kind: CloudTrailIdentityKind,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    principal_slot: usize,
) -> String {
    let mut out = JsonObjectWriter::new(256);
    let issuer_type = match identity_kind {
        CloudTrailIdentityKind::FederatedUser => "Role",
        CloudTrailIdentityKind::AssumedRole => "Role",
        CloudTrailIdentityKind::IamUser => "User",
        CloudTrailIdentityKind::Root => "Root",
        CloudTrailIdentityKind::AwsService => "AWSService",
    };
    out.field_str("type", issuer_type);
    out.field_str(
        "principalId",
        &cloudtrail_principal_id(identity_kind, account_id, principal_slot),
    );
    out.field_str(
        "arn",
        &cloudtrail_arn(
            identity_kind,
            account_id,
            principal_name,
            role_name,
            session_name,
        ),
    );
    out.field_str("accountId", account_id);
    out.field_str("userName", role_name);
    out.finish()
}

fn build_request_parameters_json(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    region: &str,
    event_index: usize,
    optional_density: u8,
) -> Option<String> {
    if action.read_only && !chance(rng, optional_density / 2) {
        return None;
    }

    let mut out = JsonObjectWriter::new(384);
    match service_kind {
        CloudTrailServiceKind::Ec2 => {
            if action.event_name == "RunInstances" {
                out.field_raw(
                    "instancesSet",
                    &format!(
                        r#"{{"items":[{{"imageId":"ami-{:08x}","instanceType":"{}","subnetId":"subnet-{:08x}","securityGroupId":"sg-{:08x}"}}]}}"#,
                        rng.u32(..),
                        pick(rng, &["t3.micro", "t3.small", "m6i.large", "c7g.large"]),
                        rng.u32(..),
                        rng.u32(..)
                    ),
                );
                out.field_raw("minCount", "1");
                out.field_raw("maxCount", "1");
                out.field_str("keyName", &format!("cloudtrail-key-{event_index:04}"));
            } else {
                out.field_raw("instanceIds", &format!(r#"["i-{:08x}"]"#, rng.u32(..)));
                out.field_bool("dryRun", chance(rng, optional_density / 2));
            }
        }
        CloudTrailServiceKind::S3 => {
            out.field_str(
                "bucketName",
                &format!("audit-{account_id}-{principal_name}"),
            );
            out.field_str(
                "key",
                &format!(
                    "cloudtrail/{region}/{event_index:04}/{}.json",
                    action.event_name.to_lowercase()
                ),
            );
            if action.data_event {
                out.field_str(
                    "x-amz-server-side-encryption",
                    pick(rng, &["AES256", "aws:kms"]),
                );
            }
        }
        CloudTrailServiceKind::Iam => {
            out.field_str("userName", principal_name);
            if action.event_name.contains("Role") {
                out.field_str("roleName", role_name);
            }
            out.field_str("path", "/service/");
        }
        CloudTrailServiceKind::Sts => {
            out.field_str(
                "roleArn",
                &format!("arn:aws:iam::{account_id}:role/{role_name}"),
            );
            out.field_str("roleSessionName", session_name);
            out.field_raw("durationSeconds", "3600");
        }
        CloudTrailServiceKind::Kms => {
            out.field_str(
                "keyId",
                &format!("arn:aws:kms:{region}:{account_id}:key/{:08x}", rng.u32(..)),
            );
            out.field_raw(
                "encryptionContext",
                &format!(
                    r#"{{"env":"{}","service":"{}"}}"#,
                    pick(rng, &["prod", "stage", "dev"]),
                    action.resource_type
                ),
            );
        }
        CloudTrailServiceKind::Lambda => {
            out.field_str(
                "functionName",
                &format!("cloudtrail-{principal_name}-{}", event_index % 16),
            );
            if action.event_name == "Invoke" {
                out.field_str("qualifier", "$LATEST");
            } else {
                out.field_bool("publish", chance(rng, optional_density));
            }
        }
        CloudTrailServiceKind::Rds => {
            out.field_str(
                "dBInstanceIdentifier",
                &format!("audit-db-{}", event_index % 128),
            );
            out.field_str(
                "dBInstanceClass",
                pick(rng, &["db.t3.micro", "db.t3.medium", "db.m6g.large"]),
            );
            out.field_str(
                "engine",
                pick(rng, &["postgres", "mysql", "aurora-postgresql"]),
            );
        }
        CloudTrailServiceKind::CloudTrail => {
            out.field_str("trailName", &format!("org-trail-{account_id}"));
            if action.event_name == "LookupEvents" {
                out.field_raw("maxResults", "50");
            }
        }
    }

    if chance(rng, optional_density / 3) {
        out.field_str("clientRequestToken", &cloudtrail_uuid_like(rng));
    }

    Some(out.finish())
}

fn build_response_elements_json(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    account_id: &str,
    principal_name: &str,
    _role_name: &str,
    _session_name: &str,
    region: &str,
    event_index: usize,
    optional_density: u8,
) -> Option<String> {
    if action.read_only && !chance(rng, optional_density / 2) {
        return None;
    }
    let mut out = JsonObjectWriter::new(256);
    match service_kind {
        CloudTrailServiceKind::Ec2 => {
            if action.event_name == "RunInstances" {
                out.field_str("reservationId", &format!("r-{:08x}", rng.u32(..)));
                out.field_raw(
                    "instancesSet",
                    &format!(
                        r#"{{"items":[{{"instanceId":"i-{:08x}","currentState":{{"code":16,"name":"running"}}}}]}}"#,
                        rng.u32(..)
                    ),
                );
            } else {
                out.field_bool("return", chance(rng, optional_density));
            }
        }
        CloudTrailServiceKind::S3 => {
            if action.data_event {
                out.field_str(
                    "x-amz-version-id",
                    &format!("{}-{:08x}", event_index, rng.u32(..)),
                );
                out.field_str("etag", &format!("\"{:08x}\"", rng.u32(..)));
            } else {
                out.field_str("bucketRegion", region);
            }
        }
        CloudTrailServiceKind::Iam => {
            out.field_raw(
                "user",
                &format!(
                    r#"{{"userName":"{}","arn":"arn:aws:iam::{}:user/{}"}}"#,
                    principal_name, account_id, principal_name
                ),
            );
        }
        CloudTrailServiceKind::Sts => {
            out.field_raw(
                "credentials",
                &format!(
                    r#"{{"accessKeyId":"AKIA{:08X}","sessionToken":"{:032x}","expiration":"2024-01-15T11:30:00Z"}}"#,
                    rng.u32(..),
                    rng.u128(..)
                ),
            );
        }
        CloudTrailServiceKind::Kms => {
            out.field_str(
                "keyId",
                &format!("arn:aws:kms:{region}:{account_id}:key/{:08x}", rng.u32(..)),
            );
            out.field_str(
                "encryptionAlgorithm",
                pick(rng, &["SYMMETRIC_DEFAULT", "RSAES_OAEP_SHA_256"]),
            );
        }
        CloudTrailServiceKind::Lambda => {
            out.field_str(
                "functionName",
                &format!("cloudtrail-{principal_name}-{}", event_index % 16),
            );
            out.field_str("responsePayload", "{\"status\":\"ok\"}");
        }
        CloudTrailServiceKind::Rds => {
            out.field_str(
                "dBInstanceIdentifier",
                &format!("audit-db-{}", event_index % 128),
            );
            out.field_str(
                "dBInstanceStatus",
                pick(rng, &["available", "modifying", "backing-up"]),
            );
        }
        CloudTrailServiceKind::CloudTrail => {
            out.field_str("trailName", &format!("org-trail-{account_id}"));
            out.field_bool("isLogging", chance(rng, optional_density));
        }
    }
    Some(out.finish())
}

fn build_resources_json(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    region: &str,
    event_index: usize,
    optional_density: u8,
) -> Option<String> {
    if !action.data_event && !chance(rng, optional_density / 2) {
        return None;
    }

    let mut items = Vec::with_capacity(2);
    items.push(build_resource_entry_json(
        rng,
        service_kind,
        action,
        account_id,
        principal_name,
        role_name,
        session_name,
        region,
        event_index,
        0,
    ));
    if action.data_event || chance(rng, optional_density / 2) {
        items.push(build_resource_entry_json(
            rng,
            service_kind,
            action,
            account_id,
            principal_name,
            role_name,
            session_name,
            region,
            event_index,
            1,
        ));
    }
    Some(format!("[{}]", items.join(",")))
}

fn build_resource_entry_json(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    region: &str,
    event_index: usize,
    ordinal: usize,
) -> String {
    let arn = cloudtrail_resource_arn(
        service_kind,
        action,
        account_id,
        principal_name,
        role_name,
        session_name,
        region,
        event_index,
        ordinal,
    );
    let mut out = JsonObjectWriter::new(256);
    out.field_str("ARN", &arn);
    out.field_str("accountId", account_id);
    out.field_str("type", action.resource_type);
    if chance(rng, 35) {
        out.field_str(
            "resourceName",
            &cloudtrail_resource_name(service_kind, action, principal_name, event_index, ordinal),
        );
    }
    out.finish()
}

fn cloudtrail_resource_arn(
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    account_id: &str,
    principal_name: &str,
    role_name: &str,
    session_name: &str,
    region: &str,
    event_index: usize,
    ordinal: usize,
) -> String {
    match service_kind {
        CloudTrailServiceKind::Ec2 => format!(
            "arn:aws:ec2:{region}:{account_id}:instance/i-{:08x}",
            0x5000_0000u32
                .wrapping_add(event_index as u32)
                .wrapping_add(ordinal as u32)
        ),
        CloudTrailServiceKind::S3 => format!(
            "arn:aws:s3:::audit-{account_id}-{principal_name}/cloudtrail/{event_index:04}/{}",
            action.event_name.to_lowercase()
        ),
        CloudTrailServiceKind::Iam => {
            if action.resource_type.contains("Role") {
                format!("arn:aws:iam::{account_id}:role/{role_name}")
            } else {
                format!("arn:aws:iam::{account_id}:user/{principal_name}")
            }
        }
        CloudTrailServiceKind::Sts => {
            format!("arn:aws:sts::{account_id}:assumed-role/{role_name}/{session_name}")
        }
        CloudTrailServiceKind::Kms => format!(
            "arn:aws:kms:{region}:{account_id}:key/{:08x}",
            0x6000_0000u32
                .wrapping_add(event_index as u32)
                .wrapping_add(ordinal as u32)
        ),
        CloudTrailServiceKind::Lambda => format!(
            "arn:aws:lambda:{region}:{account_id}:function:cloudtrail-{principal_name}-{}",
            event_index % 16
        ),
        CloudTrailServiceKind::Rds => format!(
            "arn:aws:rds:{region}:{account_id}:db:audit-db-{}",
            event_index % 128
        ),
        CloudTrailServiceKind::CloudTrail => {
            format!("arn:aws:cloudtrail:{region}:{account_id}:trail/org-trail-{account_id}")
        }
    }
}

fn cloudtrail_resource_name(
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    principal_name: &str,
    event_index: usize,
    ordinal: usize,
) -> String {
    match service_kind {
        CloudTrailServiceKind::Ec2 => format!(
            "i-{:08x}",
            0x7000_0000u32 + event_index as u32 + ordinal as u32
        ),
        CloudTrailServiceKind::S3 => format!(
            "cloudtrail/{}/{}",
            principal_name,
            action.event_name.to_lowercase()
        ),
        CloudTrailServiceKind::Iam => principal_name.to_string(),
        CloudTrailServiceKind::Sts => format!("{}-{}", principal_name, event_index % 1_000),
        CloudTrailServiceKind::Kms => format!("key/{:08x}", event_index as u32 + ordinal as u32),
        CloudTrailServiceKind::Lambda => {
            format!("cloudtrail-{}-{}", principal_name, event_index % 16)
        }
        CloudTrailServiceKind::Rds => format!("audit-db-{}", event_index % 128),
        CloudTrailServiceKind::CloudTrail => format!("org-trail-{}", event_index % 16),
    }
}

fn build_additional_event_data_json(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    action: &CloudTrailActionSpec,
    region: &str,
    optional_density: u8,
) -> Option<String> {
    if !chance(rng, optional_density / 2) && !action.data_event {
        return None;
    }
    let mut out = JsonObjectWriter::new(192);
    out.field_str("SignatureVersion", pick(rng, &["SigV4", "SigV2"]));
    out.field_str(
        "CipherSuite",
        pick(
            rng,
            &["ECDHE-RSA-AES128-GCM-SHA256", "TLS_AES_128_GCM_SHA256"],
        ),
    );
    out.field_str("tlsVersion", pick(rng, &["TLSv1.3", "TLSv1.2"]));
    out.field_str("region", region);
    if matches!(
        service_kind,
        CloudTrailServiceKind::S3 | CloudTrailServiceKind::Lambda
    ) {
        out.field_str("bucketOwner", "cloudtrail-bench");
    }
    if action.data_event {
        out.field_raw(
            "bytesTransferred",
            &format!("{}", 512 + rng.usize(..16_384)),
        );
    }
    Some(out.finish())
}

fn cloudtrail_error_pair(
    rng: &mut fastrand::Rng,
    read_only: bool,
    data_event: bool,
) -> Option<(&'static str, &'static str)> {
    let base = if data_event {
        9
    } else if read_only {
        4
    } else {
        7
    };
    if !chance(rng, base) {
        return None;
    }
    match rng.usize(..5) {
        0 => Some((
            "AccessDenied",
            "User is not authorized to perform this action",
        )),
        1 => Some(("ThrottlingException", "Rate exceeded")),
        2 => Some(("InvalidParameter", "Invalid request parameter")),
        3 => Some(("ResourceNotFoundException", "Resource not found")),
        _ => Some((
            "InternalFailure",
            "The request processing failed because of an unknown error",
        )),
    }
}

fn build_tls_details_json(
    rng: &mut fastrand::Rng,
    service_kind: CloudTrailServiceKind,
    optional_density: u8,
) -> Option<String> {
    if matches!(service_kind, CloudTrailServiceKind::CloudTrail)
        || !chance(rng, optional_density / 3)
    {
        return None;
    }
    let mut out = JsonObjectWriter::new(240);
    out.field_str("tlsVersion", pick(rng, &["TLSv1.3", "TLSv1.2"]));
    out.field_str(
        "cipherSuite",
        pick(
            rng,
            &["TLS_AES_128_GCM_SHA256", "ECDHE-RSA-AES128-GCM-SHA256"],
        ),
    );
    out.field_str(
        "clientProvidedHostHeader",
        pick(
            rng,
            &[
                "console.aws.amazon.com",
                "api.aws.amazon.com",
                "s3.amazonaws.com",
            ],
        ),
    );
    out.field_str(
        "clientProvidedPrincipal",
        pick(rng, &["console", "cli", "sdk"]),
    );
    Some(out.finish())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    use super::*;

    #[test]
    fn cloudtrail_deterministic() {
        let a = gen_cloudtrail_audit(100, 42);
        let b = gen_cloudtrail_audit(100, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn cloudtrail_valid_json() {
        let data = gen_cloudtrail_audit(120, 1);
        let text = std::str::from_utf8(&data).expect("valid utf-8");
        for line in text.lines() {
            let _: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON: {e}: {line}"));
        }
    }

    #[test]
    fn cloudtrail_has_nested_optional_fields() {
        let data = gen_cloudtrail_audit_with_profile(
            400,
            7,
            CloudTrailProfile::benchmark_default()
                .with_service_mix(CloudTrailServiceMix::SecurityHeavy)
                .with_region_mix(CloudTrailRegionMix::MultiRegion)
                .with_optional_field_density(85),
        );
        let text = std::str::from_utf8(&data).expect("valid utf-8");
        let mut user_identity_types = HashSet::new();
        let mut saw_session_context = false;
        let mut saw_resources = false;
        let mut saw_error = false;
        let mut saw_additional = false;
        let mut saw_shared = false;
        for line in text.lines() {
            let value: Value = serde_json::from_str(line).expect("valid json");
            if let Some(user_identity) = value.get("userIdentity") {
                if let Some(kind) = user_identity.get("type").and_then(Value::as_str) {
                    user_identity_types.insert(kind.to_string());
                }
                if user_identity.get("sessionContext").is_some() {
                    saw_session_context = true;
                }
            }
            if value.get("resources").is_some() {
                saw_resources = true;
            }
            if value.get("errorCode").is_some() {
                saw_error = true;
            }
            if value.get("additionalEventData").is_some() {
                saw_additional = true;
            }
            if value.get("sharedEventID").is_some() {
                saw_shared = true;
            }
        }

        assert!(user_identity_types.contains("AssumedRole"));
        assert!(user_identity_types.contains("IAMUser"));
        assert!(user_identity_types.contains("AWSService"));
        assert!(saw_session_context, "expected at least one sessionContext");
        assert!(saw_resources, "expected at least one resources array");
        assert!(saw_error, "expected at least one errorCode");
        assert!(saw_additional, "expected at least one additionalEventData");
        assert!(saw_shared, "expected at least one sharedEventID");
    }

    #[test]
    fn cloudtrail_profile_changes_mix() {
        let security = gen_cloudtrail_audit_with_profile(
            120,
            8,
            CloudTrailProfile::benchmark_default()
                .with_service_mix(CloudTrailServiceMix::SecurityHeavy),
        );
        let storage = gen_cloudtrail_audit_with_profile(
            120,
            8,
            CloudTrailProfile::benchmark_default()
                .with_service_mix(CloudTrailServiceMix::StorageHeavy),
        );
        assert_ne!(
            security, storage,
            "changing service mix should change output"
        );
    }
}
