//! Enrichment table providers — background-refreshable Arrow tables
//! that get registered in DataFusion alongside the `logs` table.
//!
//! Each provider produces an Arrow RecordBatch representing a lookup table.
//! The SqlTransform registers these as MemTables so users can JOIN against them.

use std::sync::{Arc, RwLock};

use arrow::array::StringArray;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// A named lookup table whose contents may be refreshed in the background.
///
/// Implementations store their data behind `Arc<RwLock<Option<RecordBatch>>>`.
/// The pipeline reads a snapshot once per batch (cheap RwLock read), not per row.
pub trait EnrichmentTable: Send + Sync {
    /// Table name for SQL (e.g., "k8s_pods", "host_info").
    fn name(&self) -> &str;

    /// Current snapshot. Returns None if data hasn't been loaded yet.
    fn snapshot(&self) -> Option<RecordBatch>;
}

// ---------------------------------------------------------------------------
// Static table (from config)
// ---------------------------------------------------------------------------

/// A one-row table with fixed key-value pairs from the YAML config.
///
/// ```yaml
/// enrichment:
///   - type: static
///     table_name: env
///     labels:
///       environment: production
///       cluster: us-east-1
/// ```
///
/// SQL: `SELECT *, (SELECT environment FROM env) AS env FROM logs`
/// Or:  `SELECT logs.*, e.* FROM logs CROSS JOIN env AS e`
pub struct StaticTable {
    table_name: String,
    batch: RecordBatch,
}

impl StaticTable {
    /// Create from key-value pairs.
    pub fn new(table_name: impl Into<String>, labels: &[(String, String)]) -> Self {
        let fields: Vec<Field> = labels
            .iter()
            .map(|(k, _)| Field::new(k, DataType::Utf8, false))
            .collect();
        let schema = Arc::new(Schema::new(fields));
        let columns: Vec<Arc<dyn arrow::array::Array>> = labels
            .iter()
            .map(|(_, v)| Arc::new(StringArray::from(vec![v.as_str()])) as _)
            .collect();
        let batch = RecordBatch::try_new(schema, columns).expect("static table schema mismatch");
        StaticTable {
            table_name: table_name.into(),
            batch,
        }
    }
}

impl EnrichmentTable for StaticTable {
    fn name(&self) -> &str {
        &self.table_name
    }

    fn snapshot(&self) -> Option<RecordBatch> {
        Some(self.batch.clone())
    }
}

// ---------------------------------------------------------------------------
// Host info (resolved once at startup)
// ---------------------------------------------------------------------------

/// System host metadata. One row, resolved at construction time.
///
/// Columns: hostname, os_type, os_arch
///
/// SQL: `SELECT logs.*, h.hostname FROM logs CROSS JOIN host_info AS h`
pub struct HostInfoTable {
    batch: RecordBatch,
}

impl Default for HostInfoTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HostInfoTable {
    pub fn new() -> Self {
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
        let os_type = std::env::consts::OS.to_string();
        let os_arch = std::env::consts::ARCH.to_string();

        let schema = Arc::new(Schema::new(vec![
            Field::new("hostname", DataType::Utf8, false),
            Field::new("os_type", DataType::Utf8, false),
            Field::new("os_arch", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec![hostname.as_str()])),
                Arc::new(StringArray::from(vec![os_type.as_str()])),
                Arc::new(StringArray::from(vec![os_arch.as_str()])),
            ],
        )
        .expect("host_info schema mismatch");

        HostInfoTable { batch }
    }
}

impl EnrichmentTable for HostInfoTable {
    fn name(&self) -> &str {
        "host_info"
    }

    fn snapshot(&self) -> Option<RecordBatch> {
        Some(self.batch.clone())
    }
}

// ---------------------------------------------------------------------------
// K8s pod metadata (parsed from CRI log file paths)
// ---------------------------------------------------------------------------

/// Extracts Kubernetes metadata from CRI log file paths.
///
/// CRI log path format:
///   /var/log/pods/<namespace>_<pod-name>_<pod-uid>/<container>/0.log
///
/// This is zero-cost enrichment — no K8s API calls needed.
///
/// Columns: log_path_prefix, namespace, pod_name, pod_uid, container_name
///
/// SQL:
/// ```sql
/// SELECT logs.*, k8s.namespace, k8s.pod_name
/// FROM logs
/// LEFT JOIN k8s_pods AS k8s
///   ON logs._source LIKE k8s.log_path_prefix || '%'
/// ```
pub struct K8sPathTable {
    table_name: String,
    data: Arc<RwLock<Option<RecordBatch>>>,
}

/// One parsed K8s pod entry from a CRI log path.
#[derive(Debug, Clone)]
pub struct K8sPodEntry {
    /// The directory prefix, e.g. "/var/log/pods/default_myapp-abc_uid123/container/"
    pub log_path_prefix: String,
    pub namespace: String,
    pub pod_name: String,
    pub pod_uid: String,
    pub container_name: String,
}

impl K8sPathTable {
    pub fn new(table_name: impl Into<String>) -> Self {
        K8sPathTable {
            table_name: table_name.into(),
            data: Arc::new(RwLock::new(None)),
        }
    }

    /// Parse CRI log paths and update the table. Call this when new files
    /// are discovered or on a periodic refresh.
    pub fn update_from_paths(&self, paths: &[String]) {
        let mut entries = Vec::new();
        for path in paths {
            if let Some(entry) = parse_cri_log_path(path) {
                entries.push(entry);
            }
        }
        // Deduplicate by (namespace, pod_name, container_name).
        entries.sort_by(|a, b| {
            (&a.namespace, &a.pod_name, &a.container_name).cmp(&(
                &b.namespace,
                &b.pod_name,
                &b.container_name,
            ))
        });
        entries.dedup_by(|a, b| {
            a.namespace == b.namespace
                && a.pod_name == b.pod_name
                && a.container_name == b.container_name
        });

        let batch = build_k8s_batch(&entries);
        let mut data = self.data.write().expect("k8s table lock poisoned");
        *data = Some(batch);
    }
}

impl EnrichmentTable for K8sPathTable {
    fn name(&self) -> &str {
        &self.table_name
    }

    fn snapshot(&self) -> Option<RecordBatch> {
        self.data.read().expect("k8s table lock poisoned").clone()
    }
}

/// Parse a CRI log path into K8s metadata.
///
/// Expected format: `/var/log/pods/<namespace>_<pod-name>_<pod-uid>/<container>/<N>.log`
pub fn parse_cri_log_path(path: &str) -> Option<K8sPodEntry> {
    // Find the "/pods/" segment.
    let pods_idx = path.find("/pods/")?;
    let after_pods = &path[pods_idx + 6..]; // skip "/pods/"

    // Split: <namespace>_<pod-name>_<pod-uid>/<container>/N.log
    let slash_idx = after_pods.find('/')?;
    let pod_dir = &after_pods[..slash_idx];
    let after_pod_dir = &after_pods[slash_idx + 1..];

    // Parse pod directory: namespace_podname_uid
    // UID is always 36 chars (UUID format). Work backwards.
    if pod_dir.len() < 38 {
        return None; // too short for namespace_name_uuid
    }

    // Find the last underscore before the UID (which is 36 chars from the end).
    let uid_start = pod_dir.len() - 36;
    if pod_dir.as_bytes().get(uid_start.wrapping_sub(1))? != &b'_' {
        return None;
    }
    let pod_uid = &pod_dir[uid_start..];
    let name_and_ns = &pod_dir[..uid_start - 1];

    // namespace_podname — find the first underscore (namespace can't contain underscores).
    let ns_end = name_and_ns.find('_')?;
    let namespace = &name_and_ns[..ns_end];
    let pod_name = &name_and_ns[ns_end + 1..];

    if namespace.is_empty() || pod_name.is_empty() {
        return None;
    }

    // Container name is the next path segment.
    let container_end = after_pod_dir.find('/').unwrap_or(after_pod_dir.len());
    let container_name = &after_pod_dir[..container_end];

    // Build the path prefix (directory up to and including container/).
    let prefix_end = pods_idx + 6 + slash_idx + 1 + container_end + 1;
    let log_path_prefix = if prefix_end <= path.len() {
        &path[..prefix_end]
    } else {
        path
    };

    Some(K8sPodEntry {
        log_path_prefix: log_path_prefix.to_string(),
        namespace: namespace.to_string(),
        pod_name: pod_name.to_string(),
        pod_uid: pod_uid.to_string(),
        container_name: container_name.to_string(),
    })
}

fn build_k8s_batch(entries: &[K8sPodEntry]) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("log_path_prefix", DataType::Utf8, false),
        Field::new("namespace", DataType::Utf8, false),
        Field::new("pod_name", DataType::Utf8, false),
        Field::new("pod_uid", DataType::Utf8, false),
        Field::new("container_name", DataType::Utf8, false),
    ]));

    let prefixes: Vec<&str> = entries.iter().map(|e| e.log_path_prefix.as_str()).collect();
    let namespaces: Vec<&str> = entries.iter().map(|e| e.namespace.as_str()).collect();
    let pod_names: Vec<&str> = entries.iter().map(|e| e.pod_name.as_str()).collect();
    let uids: Vec<&str> = entries.iter().map(|e| e.pod_uid.as_str()).collect();
    let containers: Vec<&str> = entries.iter().map(|e| e.container_name.as_str()).collect();

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(prefixes)),
            Arc::new(StringArray::from(namespaces)),
            Arc::new(StringArray::from(pod_names)),
            Arc::new(StringArray::from(uids)),
            Arc::new(StringArray::from(containers)),
        ],
    )
    .expect("k8s batch schema mismatch")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_cri_path() {
        let path = "/var/log/pods/default_myapp-abc-12345_a1b2c3d4-e5f6-7890-abcd-ef1234567890/nginx/0.log";
        let entry = parse_cri_log_path(path).expect("should parse");
        assert_eq!(entry.namespace, "default");
        assert_eq!(entry.pod_name, "myapp-abc-12345");
        assert_eq!(entry.pod_uid, "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
        assert_eq!(entry.container_name, "nginx");
        assert!(entry.log_path_prefix.ends_with("nginx/"));
    }

    #[test]
    fn parse_kube_system_path() {
        let path = "/var/log/pods/kube-system_coredns-5d78c9869d-abc12_f1e2d3c4-b5a6-9870-fedc-ba0987654321/coredns/0.log";
        let entry = parse_cri_log_path(path).expect("should parse");
        assert_eq!(entry.namespace, "kube-system");
        assert_eq!(entry.pod_name, "coredns-5d78c9869d-abc12");
        assert_eq!(entry.container_name, "coredns");
    }

    #[test]
    fn parse_invalid_path_returns_none() {
        assert!(parse_cri_log_path("/var/log/messages").is_none());
        assert!(parse_cri_log_path("/var/log/pods/").is_none());
        assert!(parse_cri_log_path("/var/log/pods/short/container/0.log").is_none());
    }

    #[test]
    fn static_table_creates_one_row() {
        let table = StaticTable::new(
            "env",
            &[
                ("environment".to_string(), "production".to_string()),
                ("cluster".to_string(), "us-east-1".to_string()),
            ],
        );
        assert_eq!(table.name(), "env");
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 2);
        let env_col = batch
            .column_by_name("environment")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(env_col.value(0), "production");
    }

    #[test]
    fn host_info_table_has_data() {
        let table = HostInfoTable::new();
        assert_eq!(table.name(), "host_info");
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);
        // hostname should be non-empty
        let host = batch
            .column_by_name("hostname")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(!host.value(0).is_empty());
        // os_type should be known
        let os = batch
            .column_by_name("os_type")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(!os.value(0).is_empty());
    }

    #[test]
    fn k8s_path_table_update_and_snapshot() {
        let table = K8sPathTable::new("k8s_pods");
        assert!(table.snapshot().is_none()); // initially empty

        table.update_from_paths(&[
            "/var/log/pods/default_app-a-12345_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/main/0.log"
                .to_string(),
            "/var/log/pods/monitoring_prom-0_11111111-2222-3333-4444-555555555555/prometheus/0.log"
                .to_string(),
        ]);

        let batch = table.snapshot().expect("should have data");
        assert_eq!(batch.num_rows(), 2);

        let ns = batch
            .column_by_name("namespace")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        // Sorted by namespace
        assert_eq!(ns.value(0), "default");
        assert_eq!(ns.value(1), "monitoring");
    }

    #[test]
    fn k8s_path_table_deduplicates() {
        let table = K8sPathTable::new("k8s_pods");
        table.update_from_paths(&[
            "/var/log/pods/default_app-12345_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/main/0.log"
                .to_string(),
            "/var/log/pods/default_app-12345_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/main/1.log"
                .to_string(),
        ]);
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1); // deduplicated
    }
}
