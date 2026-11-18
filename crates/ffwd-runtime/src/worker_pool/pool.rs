include!("pool/lifecycle.rs");
include!("pool/health.rs");
include!("pool/types.rs");
include!("pool/retry_grace.rs");
include!("pool/runtime.rs");

#[cfg(test)]
mod tests {
    include!("pool/tests/dispatch.rs");
    include!("pool/tests/factories.rs");
    include!("pool/tests/runtime.rs");
    include!("pool/tests/concurrency.rs");
}
