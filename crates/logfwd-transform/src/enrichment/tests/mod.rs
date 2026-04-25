#[cfg(test)]
mod tests {
    use crate::enrichment::*;
    use arrow::array::Array;

    fn csv_string_column<'a>(
        batch: &'a RecordBatch,
        name: &str,
    ) -> &'a arrow::array::StringViewArray {
        batch
            .column_by_name(name)
            .unwrap_or_else(|| panic!("missing CSV column: {name}"))
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap_or_else(|| panic!("CSV column {name} should be Utf8View"))
    }

    // -- CRI path parsing ---------------------------------------------------

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
    fn parse_hyphenated_namespace() {
        let path = "/var/log/pods/my-team-prod_api-server-xyz_abcdefab-1234-5678-9abc-def012345678/app/0.log";
        let entry = parse_cri_log_path(path).expect("should parse");
        // Hyphens in namespace are fine — only underscores separate ns from pod name.
        assert_eq!(entry.namespace, "my-team-prod");
        assert_eq!(entry.pod_name, "api-server-xyz");
    }

    #[test]
    fn parse_pod_name_with_underscores() {
        // Pod names can contain underscores in some edge cases (StatefulSets, etc.)
        // Our parser uses the FIRST underscore as ns/pod separator.
        let path =
            "/var/log/pods/ns_pod_with_underscores_abcdefab-1234-5678-9abc-def012345678/c/0.log";
        let entry = parse_cri_log_path(path).expect("should parse");
        assert_eq!(entry.namespace, "ns");
        assert_eq!(entry.pod_name, "pod_with_underscores");
    }

    #[test]
    fn parse_invalid_paths() {
        assert!(parse_cri_log_path("/var/log/messages").is_none());
        assert!(parse_cri_log_path("/var/log/pods/").is_none());
        assert!(parse_cri_log_path("/var/log/pods/short/container/0.log").is_none());
        assert!(parse_cri_log_path("").is_none());
        assert!(parse_cri_log_path("/var/log/pods/a/b/0.log").is_none()); // no valid UUID
    }

    // -- Static table -------------------------------------------------------

    #[test]
    fn static_table_one_row() {
        let table = StaticTable::new(
            "env",
            &[
                ("environment".to_string(), "production".to_string()),
                ("cluster".to_string(), "us-east-1".to_string()),
            ],
        )
        .expect("valid labels");
        assert_eq!(table.name(), "env");
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 2);
        let env = batch
            .column_by_name("environment")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(env.value(0), "production");
    }

    #[test]
    fn static_table_single_label() {
        let table =
            StaticTable::new("t", &[("key".to_string(), "value".to_string())]).expect("valid");
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 1);
    }

    #[test]
    fn static_table_empty_labels_returns_error() {
        let result = StaticTable::new("t", &[]);
        let err = result.err().expect("empty labels should return Err");
        assert_eq!(
            err.to_string(),
            "enrichment error: StaticTable requires at least one label",
            "error message must identify the cause"
        );
    }

    // -- Host info ----------------------------------------------------------

    #[test]
    fn host_info_has_all_columns() {
        let table = HostInfoTable::new();
        assert_eq!(table.name(), "host_info");
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 3);
        for col_name in &["hostname", "os_type", "os_arch"] {
            let col = batch
                .column_by_name(col_name)
                .unwrap_or_else(|| panic!("missing column: {col_name}"))
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            assert!(!col.value(0).is_empty(), "{col_name} should be non-empty");
        }
    }

    // -- K8s path table -----------------------------------------------------

    #[test]
    fn k8s_path_table_starts_with_empty_batch() {
        let table = K8sPathTable::new("k8s_pods");
        let batch = table.snapshot().expect("should have empty batch");
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.num_columns(), 5); // log_path_prefix, namespace, pod_name, pod_uid, container_name
    }

    #[test]
    fn k8s_path_table_update_and_snapshot() {
        let table = K8sPathTable::new("k8s_pods");
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
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn k8s_path_table_ignores_invalid_paths() {
        let table = K8sPathTable::new("k8s_pods");
        table.update_from_paths(&[
            "/var/log/messages".to_string(),
            "/var/log/pods/default_app-12345_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/main/0.log"
                .to_string(),
            "/not/a/cri/path".to_string(),
        ]);
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1); // only the valid one
    }

    #[test]
    fn k8s_path_table_refresh_replaces_data() {
        let table = K8sPathTable::new("k8s_pods");
        table.update_from_paths(&[
            "/var/log/pods/default_app-a_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/c/0.log".to_string(),
        ]);
        assert_eq!(table.snapshot().unwrap().num_rows(), 1);

        // Refresh with different data.
        table.update_from_paths(&[
            "/var/log/pods/ns1_pod1_11111111-2222-3333-4444-555555555555/c/0.log".to_string(),
            "/var/log/pods/ns2_pod2_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/c/0.log".to_string(),
        ]);
        assert_eq!(table.snapshot().unwrap().num_rows(), 2);
    }

    // -- CSV file table -----------------------------------------------------

    #[test]
    fn csv_load_basic() {
        let csv_data = b"hostname,owner,team\nweb-1,alice,platform\napi-2,bob,backend\n";
        let table = CsvFileTable::new("assets", "/fake/path.csv");
        let rows = table.load_from_reader(&csv_data[..]).unwrap();
        assert_eq!(rows, 2);
        assert_eq!(table.name(), "assets");

        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 3);
        assert_eq!(batch.schema().field(0).data_type(), &DataType::Utf8View);

        let hostname = csv_string_column(&batch, "hostname");
        assert_eq!(hostname.value(0), "web-1");
        assert_eq!(hostname.value(1), "api-2");

        let team = csv_string_column(&batch, "team");
        assert_eq!(team.value(0), "platform");
        assert_eq!(team.value(1), "backend");
    }

    #[test]
    fn csv_with_missing_fields() {
        let csv_data = b"a,b,c\n1,2,3\n4,5\n"; // row 2 missing column c
        let table = CsvFileTable::new("t", "/fake");
        table.load_from_reader(&csv_data[..]).unwrap();
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let c = csv_string_column(&batch, "c");
        assert_eq!(c.value(0), "3");
        assert!(c.is_null(1)); // padded with NULL
    }

    #[test]
    fn csv_empty_cells_are_empty_strings() {
        let csv_data = b"a,b,c\n1,,3\n,5,\n";
        let table = CsvFileTable::new("t", "/fake");
        table.load_from_reader(&csv_data[..]).unwrap();
        let batch = table.snapshot().unwrap();

        let a = csv_string_column(&batch, "a");
        let b = csv_string_column(&batch, "b");
        let c = csv_string_column(&batch, "c");
        assert_eq!(a.value(1), "");
        assert_eq!(b.value(0), "");
        assert_eq!(c.value(1), "");
    }

    #[test]
    fn csv_quoted_commas_stay_in_cell() {
        let csv_data = b"host,note\nweb-1,\"hello,team\"\napi-2,\"x,y,z\"\n";
        let table = CsvFileTable::new("t", "/fake");
        table.load_from_reader(&csv_data[..]).unwrap();
        let batch = table.snapshot().unwrap();

        let note = csv_string_column(&batch, "note");
        assert_eq!(note.value(0), "hello,team");
        assert_eq!(note.value(1), "x,y,z");
    }

    #[test]
    fn csv_all_missing_column_preserves_header_as_nulls() {
        let csv_data = b"a,b\n1\n2\n";
        let table = CsvFileTable::new("t", "/fake");
        table.load_from_reader(&csv_data[..]).unwrap();
        let batch = table.snapshot().unwrap();

        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.schema().field(1).name(), "b");
        assert_eq!(batch.schema().field(1).data_type(), &DataType::Utf8View);
        let b = csv_string_column(&batch, "b");
        assert!(b.is_null(0));
        assert!(b.is_null(1));
    }

    #[test]
    fn csv_columnar_builder_matches_legacy_values() {
        let csv_data = b"a,b,c\n1,2,3\n4,,\n5\n";
        let columnar = read_csv_to_batch(&csv_data[..]).unwrap();
        let legacy = read_csv_to_legacy_batch_for_test(&csv_data[..]).unwrap();

        assert_eq!(columnar.num_rows(), legacy.num_rows());
        assert_eq!(columnar.num_columns(), legacy.num_columns());
        for field in legacy.schema().fields() {
            let name = field.name();
            let actual = csv_string_column(&columnar, name);
            let expected = legacy
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for row in 0..legacy.num_rows() {
                assert_eq!(
                    actual.is_null(row),
                    expected.is_null(row),
                    "null mismatch for {name} row {row}",
                );
                if !expected.is_null(row) {
                    assert_eq!(actual.value(row), expected.value(row));
                }
            }
        }
    }

    #[test]
    fn csv_empty_file_fails() {
        let table = CsvFileTable::new("t", "/fake");
        let result = table.load_from_reader(&b""[..]);
        assert!(result.is_err());
    }

    #[test]
    fn csv_empty_header_fails() {
        let csv_data = b"host,,team\nweb-1,alice,platform\n";
        let table = CsvFileTable::new("t", "/fake");
        let result = table.load_from_reader(&csv_data[..]);
        let err = result.expect_err("empty header should return Err");
        assert!(err.to_string().contains("empty header name"));
    }

    #[test]
    fn csv_duplicate_header_fails() {
        let csv_data = b"host,host\nweb-1,web-2\n";
        let table = CsvFileTable::new("t", "/fake");
        let result = table.load_from_reader(&csv_data[..]);
        let err = result.expect_err("duplicate header should return Err");
        assert!(err.to_string().contains("duplicate header name"));
    }

    #[test]
    fn csv_reload_replaces_data() {
        let table = CsvFileTable::new("t", "/fake");
        table.load_from_reader(&b"col\nval1\n"[..]).unwrap();
        assert_eq!(table.snapshot().unwrap().num_rows(), 1);

        table
            .load_from_reader(&b"col\nval1\nval2\nval3\n"[..])
            .unwrap();
        assert_eq!(table.snapshot().unwrap().num_rows(), 3);
    }

    #[test]
    fn csv_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("test.csv");
        std::fs::write(&csv_path, "ip,region\n10.0.0.1,us-east\n10.0.0.2,eu-west\n").unwrap();

        let table = CsvFileTable::new("ips", &csv_path);
        let rows = table.reload().unwrap();
        assert_eq!(rows, 2);

        let batch = table.snapshot().unwrap();
        let region = csv_string_column(&batch, "region");
        assert_eq!(region.value(0), "us-east");
        assert_eq!(region.value(1), "eu-west");
    }

    // -- JSON Lines file table ----------------------------------------------

    #[test]
    fn jsonl_load_basic() {
        let data =
            b"{\"ip\":\"10.0.0.1\",\"owner\":\"alice\"}\n{\"ip\":\"10.0.0.2\",\"owner\":\"bob\"}\n";
        let table = JsonLinesFileTable::new("ip_owners", "/fake");
        let rows = table.load_from_reader(&data[..]).unwrap();
        assert_eq!(rows, 2);

        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 2);

        let ip = batch
            .column_by_name("ip")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(ip.value(0), "10.0.0.1");
        assert_eq!(ip.value(1), "10.0.0.2");
    }

    #[test]
    fn jsonl_union_schema() {
        // Row 1 has {a, b}, row 2 has {b, c} — result should have {a, b, c}.
        let data = b"{\"a\":\"1\",\"b\":\"2\"}\n{\"b\":\"3\",\"c\":\"4\"}\n";
        let table = JsonLinesFileTable::new("t", "/fake");
        table.load_from_reader(&data[..]).unwrap();

        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 3);

        let a = batch
            .column_by_name("a")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(a.value(0), "1");
        assert!(a.is_null(1)); // row 2 doesn't have "a"

        let c = batch
            .column_by_name("c")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(c.is_null(0)); // row 1 doesn't have "c"
        assert_eq!(c.value(1), "4");
    }

    #[test]
    fn jsonl_non_string_values_stringified() {
        let data = b"{\"name\":\"web\",\"port\":8080,\"active\":true}\n";
        let table = JsonLinesFileTable::new("t", "/fake");
        table.load_from_reader(&data[..]).unwrap();

        let batch = table.snapshot().unwrap();
        let port = batch
            .column_by_name("port")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(port.value(0), "8080");

        let active = batch
            .column_by_name("active")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(active.value(0), "true");
    }

    #[test]
    fn jsonl_skips_blank_lines() {
        let data = b"{\"a\":\"1\"}\n\n{\"a\":\"2\"}\n\n";
        let table = JsonLinesFileTable::new("t", "/fake");
        table.load_from_reader(&data[..]).unwrap();
        assert_eq!(table.snapshot().unwrap().num_rows(), 2);
    }

    #[test]
    fn jsonl_empty_fails() {
        let table = JsonLinesFileTable::new("t", "/fake");
        let result = table.load_from_reader(&b""[..]);
        assert!(result.is_err());
    }

    // -- Trait object dispatch -----------------------------------------------

    #[test]
    fn trait_object_dispatch() {
        let tables: Vec<Box<dyn EnrichmentTable>> = vec![
            Box::new(
                StaticTable::new("env", &[("k".to_string(), "v".to_string())])
                    .expect("valid labels"),
            ),
            Box::new(HostInfoTable::new()),
            Box::new(K8sPathTable::new("k8s_pods")),
        ];

        assert_eq!(tables[0].name(), "env");
        assert!(tables[0].snapshot().is_some());
        assert_eq!(tables[1].name(), "host_info");
        assert!(tables[1].snapshot().is_some());
        assert_eq!(tables[2].name(), "k8s_pods");
        let batch = tables[2].snapshot().expect("should have empty batch");
        assert_eq!(batch.num_rows(), 0); // empty until update_from_paths
    }

    // -- Concurrent access --------------------------------------------------

    #[test]
    fn concurrent_read_write() {
        let table = Arc::new(K8sPathTable::new("k8s_pods"));

        // Spawn a writer.
        let writer = Arc::clone(&table);
        let handle = std::thread::spawn(move || {
            for i in 0..10 {
                let path =
                    format!("/var/log/pods/ns_pod-{i}_aaaaaaaa-bbbb-cccc-dddd-{i:012x}/c/0.log");
                writer.update_from_paths(&[path]);
            }
        });

        // Read concurrently.
        for _ in 0..100 {
            let _ = table.snapshot(); // should not panic
        }

        handle.join().unwrap();
        // Final state should have data.
        assert!(table.snapshot().is_some());
    }

    // -- ReloadableGeoDb ----------------------------------------------------

    struct FixedGeoDb(GeoResult);
    impl GeoDatabase for FixedGeoDb {
        fn lookup(&self, _ip: &str) -> Option<GeoResult> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn reloadable_geo_db_initial_lookup() {
        let result = GeoResult {
            country_code: Some("US".to_string()),
            ..Default::default()
        };
        let db = Arc::new(FixedGeoDb(result));
        let reloadable = Arc::new(ReloadableGeoDb::new(db));
        let got = reloadable.lookup("1.2.3.4").unwrap();
        assert_eq!(got.country_code.as_deref(), Some("US"));
    }

    #[test]
    fn reloadable_geo_db_swap_replaces_backend() {
        let first = Arc::new(FixedGeoDb(GeoResult {
            country_code: Some("US".to_string()),
            ..Default::default()
        }));
        let reloadable = Arc::new(ReloadableGeoDb::new(first));
        let handle = reloadable.reload_handle();

        let second = Arc::new(FixedGeoDb(GeoResult {
            country_code: Some("DE".to_string()),
            ..Default::default()
        }));
        handle.replace(second);

        let got = reloadable.lookup("1.2.3.4").unwrap();
        assert_eq!(got.country_code.as_deref(), Some("DE"));
    }

    #[test]
    fn reloadable_geo_db_concurrent_reads() {
        let db = Arc::new(FixedGeoDb(GeoResult {
            country_code: Some("AU".to_string()),
            ..Default::default()
        }));
        let reloadable = Arc::new(ReloadableGeoDb::new(db));
        let handle = reloadable.reload_handle();

        let reader = Arc::clone(&reloadable);
        let reader_thread = std::thread::spawn(move || {
            for _ in 0..100 {
                let _ = reader.lookup("8.8.8.8");
            }
        });

        // Swap pointer while reader is running.
        handle.replace(Arc::new(FixedGeoDb(GeoResult {
            country_code: Some("GB".to_string()),
            ..Default::default()
        })));

        reader_thread.join().unwrap();
    }

    // -- EnvTable -----------------------------------------------------------

    #[test]
    fn env_table_reads_prefix() {
        // SAFETY: test sets and clears env vars; must not run in parallel with other
        // tests that read the same vars.
        unsafe {
            std::env::set_var("LOGFWD_TEST_CLUSTER", "prod");
            std::env::set_var("LOGFWD_TEST_REGION", "us-east-1");
        }

        let table = EnvTable::from_prefix("deploy", "LOGFWD_TEST_").expect("should succeed");
        assert_eq!(table.name(), "deploy");
        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);

        // Columns exist for the two vars we set (plus any pre-existing matches).
        assert!(batch.column_by_name("cluster").is_some());
        assert!(batch.column_by_name("region").is_some());

        let cluster = batch
            .column_by_name("cluster")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(cluster.value(0), "prod");

        // SAFETY: `remove_var` is unsafe because it mutates process-global
        // state and is unsound under concurrent access. This is safe here
        // because `cargo nextest` runs each test in its own process, and
        // these variables use a unique `LOGFWD_TEST_` prefix not read by
        // any other test.
        unsafe {
            std::env::remove_var("LOGFWD_TEST_CLUSTER");
            std::env::remove_var("LOGFWD_TEST_REGION");
        }
    }

    #[test]
    fn env_table_no_match_returns_error() {
        let result = EnvTable::from_prefix("nothing", "LOGFWD_NONEXISTENT_PREFIX_XYZZY_12345_");
        assert!(result.is_err());
    }

    #[test]
    #[cfg(unix)] // Windows env vars are case-insensitive; both names map to one var.
    fn env_table_rejects_duplicate_columns_after_lowercasing() {
        // Set env vars that collide after lowercasing the suffix.
        // SAFETY: test is run single-threaded via `cargo nextest` (which isolates
        // each test in its own process) or `--test-threads=1`. The vars use a
        // unique prefix (LOGFWD_DUPTEST_) to avoid collisions with real env.
        unsafe {
            std::env::set_var("LOGFWD_DUPTEST_FOO", "a");
            std::env::set_var("LOGFWD_DUPTEST_foo", "b");
        }
        let result = EnvTable::from_prefix("dup_test", "LOGFWD_DUPTEST_");
        // Clean up before asserting.
        // SAFETY: this removes only the unique test variables set above.
        unsafe {
            std::env::remove_var("LOGFWD_DUPTEST_FOO");
            std::env::remove_var("LOGFWD_DUPTEST_foo");
        }
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("duplicate column name"), "got: {msg}");
    }

    // -- ProcessInfoTable -------------------------------------------------------

    #[test]
    fn process_info_has_expected_columns() {
        let table = ProcessInfoTable::new();
        let batch = table.snapshot().expect("should have snapshot");
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column_by_name("agent_name").is_some());
        assert!(batch.column_by_name("agent_version").is_some());
        assert!(batch.column_by_name("pid").is_some());
        assert!(batch.column_by_name("start_time").is_some());

        let name_col = batch
            .column_by_name("agent_name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name_col.value(0), "logfwd");

        let pid_col = batch
            .column_by_name("pid")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let pid: u32 = pid_col.value(0).parse().expect("pid should be numeric");
        assert!(pid > 0);
    }

    #[test]
    fn process_info_start_time_is_iso8601() {
        let table = ProcessInfoTable::new();
        let batch = table.snapshot().unwrap();
        let ts = batch
            .column_by_name("start_time")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0)
            .to_string();
        // Should look like "2026-04-12T06:20:13Z"
        assert!(ts.ends_with('Z'), "expected UTC: {ts}");
        assert_eq!(ts.len(), 20, "expected ISO 8601 length: {ts}");
    }

    #[test]
    fn process_info_table_name() {
        let table = ProcessInfoTable::new();
        assert_eq!(table.name(), "process_info");
    }

    // -- epoch_days_to_ymd -------------------------------------------------------

    #[test]
    fn epoch_days_known_dates() {
        // Unix epoch: 1970-01-01
        assert_eq!(epoch_days_to_ymd(0), (1970, 1, 1));
        // 2000-01-01 is day 10957
        assert_eq!(epoch_days_to_ymd(10957), (2000, 1, 1));
        // 2024-02-29 (leap day) is day 19782
        assert_eq!(epoch_days_to_ymd(19782), (2024, 2, 29));
    }

    // -- KvFileTable -------------------------------------------------------

    #[test]
    fn kv_file_parses_os_release_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("os-release");
        std::fs::write(
            &path,
            r#"# This is a comment
    NAME="Ubuntu"
    VERSION_ID="22.04"
    ID=ubuntu
    PRETTY_NAME="Ubuntu 22.04.3 LTS"
    "#,
        )
        .unwrap();

        let table = KvFileTable::new("os", &path);
        let n = table.reload().unwrap();
        assert_eq!(n, 4);

        let batch = table.snapshot().unwrap();
        assert_eq!(batch.num_rows(), 1);

        let name_col = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name_col.value(0), "Ubuntu");

        let id_col = batch
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(id_col.value(0), "ubuntu");
    }

    #[test]
    fn kv_file_handles_single_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.env");
        std::fs::write(&path, "KEY='single quoted'\n").unwrap();

        let table = KvFileTable::new("test", &path);
        table.reload().unwrap();
        let batch = table.snapshot().unwrap();
        let val = batch
            .column_by_name("key")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0);
        assert_eq!(val, "single quoted");
    }

    #[test]
    fn kv_file_handles_single_char_quoted_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.env");
        // A single quote character as the entire value — previously panicked.
        std::fs::write(&path, "KEY=\"\nOTHER=ok\n").unwrap();

        let table = KvFileTable::new("test", &path);
        table.reload().unwrap();
        let batch = table.snapshot().unwrap();
        // The single `"` should be kept as-is (not stripped).
        let val = batch
            .column_by_name("key")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0);
        assert_eq!(val, "\"");
    }

    #[test]
    fn kv_file_empty_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.env");
        std::fs::write(&path, "# only comments\n\n").unwrap();

        let table = KvFileTable::new("empty", &path);
        assert!(table.reload().is_err());
    }

    #[test]
    fn kv_file_missing_file_returns_error() {
        let table = KvFileTable::new("missing", Path::new("/nonexistent/file.env"));
        assert!(table.reload().is_err());
    }

    #[test]
    fn kv_file_reload_updates_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.env");
        std::fs::write(&path, "VERSION=1\n").unwrap();

        let table = KvFileTable::new("ver", &path);
        table.reload().unwrap();
        let v1 = table
            .snapshot()
            .unwrap()
            .column_by_name("version")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0)
            .to_string();
        assert_eq!(v1, "1");

        std::fs::write(&path, "VERSION=2\n").unwrap();
        table.reload().unwrap();
        let v2 = table
            .snapshot()
            .unwrap()
            .column_by_name("version")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0)
            .to_string();
        assert_eq!(v2, "2");
    }

    // -- NetworkInfoTable -------------------------------------------------------

    #[test]
    fn network_info_has_expected_columns() {
        let table = NetworkInfoTable::new();
        let batch = table.snapshot().expect("should have snapshot");
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column_by_name("hostname").is_some());
        assert!(batch.column_by_name("primary_ipv4").is_some());
        assert!(batch.column_by_name("primary_ipv6").is_some());
        assert!(batch.column_by_name("all_ipv4").is_some());
        assert!(batch.column_by_name("all_ipv6").is_some());

        // Hostname should be non-empty on any real system
        let hostname = batch
            .column_by_name("hostname")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0);
        assert!(!hostname.is_empty());
    }

    #[test]
    fn network_info_table_name() {
        let table = NetworkInfoTable::new();
        assert_eq!(table.name(), "network_info");
    }

    // -- format_ipv6_hex -------------------------------------------------------

    #[test]
    fn format_ipv6_hex_known_address() {
        // 2001:0db8:0000:0000:0000:0000:0000:0001
        let hex = "20010db8000000000000000000000001";
        let formatted = format_ipv6_hex(hex).unwrap();
        assert_eq!(formatted, "2001:db8:0:0:0:0:0:1");
    }

    #[test]
    fn format_ipv6_hex_all_zeros() {
        let hex = "00000000000000000000000000000000";
        let formatted = format_ipv6_hex(hex).unwrap();
        assert_eq!(formatted, "0:0:0:0:0:0:0:0");
    }

    #[test]
    fn format_ipv6_hex_wrong_length() {
        assert!(format_ipv6_hex("abc").is_none());
    }

    // -- ContainerInfoTable -----------------------------------------------------

    #[test]
    fn container_info_has_expected_columns() {
        let table = ContainerInfoTable::new();
        let batch = table.snapshot().expect("should have snapshot");
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column_by_name("container_id").is_some());
        assert!(batch.column_by_name("container_runtime").is_some());
    }

    #[test]
    fn container_info_table_name() {
        let table = ContainerInfoTable::new();
        assert_eq!(table.name(), "container_info");
    }

    #[test]
    fn parse_cgroup_v1_docker_path() {
        let content =
            "12:memory:/docker/abc123def456abc123def456abc123def456abc123def456abc123def456abc1\n";
        let result = parse_cgroup_for_container(content);
        assert!(result.is_some());
        let (id, runtime) = result.unwrap();
        assert_eq!(runtime, "docker");
        assert_eq!(id.len(), 64);
    }

    #[test]
    fn parse_cgroup_v2_docker_systemd_scope() {
        let id_hex = "a1b2c3d4".repeat(8); // 64 hex chars
        let content = format!("0::/system.slice/docker-{id_hex}.scope\n");
        let result = parse_cgroup_for_container(&content);
        assert!(result.is_some());
        let (id, runtime) = result.unwrap();
        assert_eq!(runtime, "docker");
        assert_eq!(id, id_hex);
    }

    #[test]
    fn parse_cgroup_not_container() {
        let content = "0::/init.scope\n";
        let result = parse_cgroup_for_container(content);
        assert!(result.is_none());
    }

    // -- K8sClusterInfoTable ----------------------------------------------------

    #[test]
    fn k8s_cluster_info_has_expected_columns() {
        let table = K8sClusterInfoTable::new();
        let batch = table.snapshot().expect("should have snapshot");
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column_by_name("namespace").is_some());
        assert!(batch.column_by_name("pod_name").is_some());
        assert!(batch.column_by_name("node_name").is_some());
        assert!(batch.column_by_name("service_account").is_some());
        assert!(batch.column_by_name("cluster_name").is_some());
    }

    #[test]
    fn k8s_cluster_info_table_name() {
        let table = K8sClusterInfoTable::new();
        assert_eq!(table.name(), "k8s_cluster_info");
    }

    // -- is_hex_container_id ----------------------------------------------------

    #[test]
    fn hex_container_id_valid() {
        let id = "a".repeat(64);
        assert!(is_hex_container_id(&id));
    }

    #[test]
    fn hex_container_id_too_short() {
        assert!(!is_hex_container_id("abc123"));
    }

    #[test]
    fn hex_container_id_non_hex() {
        let id = "g".repeat(64);
        assert!(!is_hex_container_id(&id));
    }
}
