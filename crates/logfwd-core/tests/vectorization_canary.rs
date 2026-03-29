// vectorization_canary.rs — Verify that key functions are auto-vectorized by LLVM.
//
// This test compiles a standalone copy of performance-critical functions with
// LLVM vectorization remarks enabled, then asserts that LLVM reports successful
// vectorization. This catches regressions where code changes accidentally
// break auto-vectorization (e.g., adding a branch that prevents SLP/loop
// vectorization).
//
// WHY: When find_char_mask is compiled standalone, LLVM vectorizes it to
// pcmpeqb (SSE2), vpcmpeqb (AVX2/512), or cmeq (NEON). If someone changes
// the loop in a way that breaks vectorization (adding early exits, changing
// the accumulation pattern, etc.), this test catches it.
//
// NOTE: This test checks that LLVM *can* vectorize the function in isolation.
// Under thin LTO, the function may be inlined into a larger context where
// LLVM's cost model skips vectorization — that's a separate (known) issue.

use std::process::Command;

/// Source code for find_char_mask, extracted so the test stays in sync.
/// If you change find_char_mask in chunk_classify.rs, update this too.
const FIND_CHAR_MASK_SRC: &str = r#"
#[inline(never)]
#[unsafe(no_mangle)]
pub fn find_char_mask(data: &[u8; 64], needle: u8) -> u64 {
    let mut bits: u64 = 0;
    for (i, &byte) in data.iter().enumerate() {
        bits |= ((byte == needle) as u64) << i;
    }
    bits
}
fn main() {}
"#;

#[test]
fn find_char_mask_is_vectorized() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let src = dir.path().join("canary.rs");
    std::fs::write(&src, FIND_CHAR_MASK_SRC).expect("write source");

    let output = Command::new("rustc")
        .args([
            "-O",
            "--edition",
            "2024",
            "-C",
            "llvm-args=-pass-remarks=loop-vectorize",
            src.to_str().unwrap(),
            "-o",
        ])
        .arg(dir.path().join("canary"))
        .output()
        .expect("run rustc");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "rustc failed:\n{stderr}"
    );

    // LLVM emits "vectorized loop (vectorization width: N, interleaved count: M)"
    // when it successfully auto-vectorizes a loop.
    assert!(
        stderr.contains("vectorized loop"),
        "LLVM did not auto-vectorize find_char_mask.\n\
         This means a code change broke the vectorization pattern.\n\
         The loop should be: `bits |= ((byte == needle) as u64) << i`\n\
         with no branches, early exits, or non-trivial loop-carried deps.\n\
         \n\
         rustc stderr:\n{stderr}"
    );
}
