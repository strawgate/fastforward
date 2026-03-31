use std::io::Write;

pub fn generate_simple(n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n * 180);
    let levels = ["INFO", "DEBUG", "WARN", "ERROR"];
    let paths = ["/api/v1/users", "/api/v1/orders", "/api/v2/products", "/health", "/api/v1/auth"];
    for i in 0..n {
        let _ = write!(
            buf,
            r#"{{"timestamp":"2024-01-15T10:30:00.{:03}Z","level":"{}","message":"request handled GET {}/{}","duration_ms":{},"request_id":"{:016x}","service":"myapp"}}"#,
            i % 1000,
            levels[i % 4],
            paths[i % 5],
            10000 + (i * 7) % 90000,
            1 + (i * 13) % 500,
            (i as u64).wrapping_mul(0x517cc1b727220a95),
        );
        buf.push(b'\n');
    }
    buf
}

pub fn generate_wide(n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n * 600);
    let levels = ["INFO", "DEBUG", "WARN", "ERROR"];
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];
    let regions = ["us-east-1", "us-west-2", "eu-west-1", "ap-southeast-1"];
    let namespaces = ["default", "kube-system", "monitoring", "logging"];
    for i in 0..n {
        let _ = write!(
            buf,
            r#"{{"timestamp":"2024-01-15T10:30:00.{:03}Z","level":"{}","message":"request {}","duration_ms":{},"service":"myapp","host":"node-{}","pod":"app-{:04}","namespace":"{}","method":"{}","status_code":{},"region":"{}","user_id":"user-{}","trace_id":"{:032x}","response_bytes":{},"latency_p99_ms":{},"error_count":{},"cache_hit":{},"db_query_ms":{},"upstream":"svc-{}","version":"v{}.{}"}}"#,
            i % 1000, levels[i % 4], i, 1 + (i * 13) % 500,
            i % 10, i % 100, namespaces[i % 4], methods[i % 5],
            [200, 201, 400, 404, 500][i % 5], regions[i % 4],
            i % 1000, (i as u64).wrapping_mul(0x517cc1b727220a95),
            100 + (i * 37) % 10000, 10 + (i * 11) % 1000,
            if i % 20 == 0 { 1 } else { 0 },
            if i % 3 == 0 { "true" } else { "false" },
            (i * 7) % 200, i % 4, 1 + i % 5, i % 10,
        );
        buf.push(b'\n');
    }
    buf
}

pub fn generate_narrow(n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n * 60);
    let levels = ["INFO", "DEBUG", "WARN", "ERROR"];
    for i in 0..n {
        let _ = write!(buf, r#"{{"ts":"{}","lvl":"{}","msg":"event {}"}}"#, i, levels[i % 4], i);
        buf.push(b'\n');
    }
    buf
}
