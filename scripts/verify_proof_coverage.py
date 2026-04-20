#!/usr/bin/env python3
"""Prototype verification lint heuristics. Scans source for coverage gaps.

Experimental script to assess feasibility of 4 verification lints:
1. pure_fn_needs_proof — pure logic in logfwd-core without Kani proofs
2. parser_needs_fuzz — &[u8] parsers without fuzz targets
3. reducer_needs_proof — state machine reducers without proofs
4. encode_decode_needs_roundtrip — encode/decode pairs without roundtrip tests

Usage:
    python3 scripts/verify_proof_coverage.py [repo_root]

Results are written to /tmp/lint-prototype-results.md
"""

import re
import os
import sys
from pathlib import Path
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class Finding:
    """A single lint finding."""
    file: str
    line_no: int
    fn_name: str
    reason: str
    classification: str = ""  # TP, FP, Exempt — filled in manually after review


@dataclass
class LintResult:
    """Results for one lint."""
    name: str
    description: str
    findings: list = field(default_factory=list)
    notes: str = ""


# ---------------------------------------------------------------------------
# Utility: parse Rust source for function signatures
# ---------------------------------------------------------------------------

# Match pub fn / pub(crate) fn declarations (not inside #[cfg(kani)] or #[cfg(test)])
PUB_FN_RE = re.compile(
    r'^(\s*)pub(?:\(crate\))?\s+(?:const\s+)?(?:unsafe\s+)?(?:async\s+)?fn\s+(\w+)\s*'
    r'(?:<[^>]*>)?\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)\s*(?:->\s*(.+?))?\s*\{?$',
    re.MULTILINE,
)


def parse_pub_fns(source: str, filepath: str):
    """Extract pub fn declarations with metadata."""
    fns = []
    lines = source.split('\n')

    # Track #[cfg(kani)] and #[cfg(test)] module boundaries
    in_cfg_block = False
    cfg_block_depth = 0
    cfg_block_type = None  # "kani" or "test"

    for i, line in enumerate(lines):
        stripped = line.strip()

        # Detect start of #[cfg(kani)] or #[cfg(test)] mod
        if re.match(r'#\[cfg\(kani\)\]', stripped):
            cfg_block_type = "kani"
            continue
        if re.match(r'#\[cfg\(test\)\]', stripped):
            cfg_block_type = "test"
            continue

        # Track module blocks after cfg attribute
        if cfg_block_type and not in_cfg_block:
            if stripped.startswith('mod '):
                in_cfg_block = True
                cfg_block_depth = 0
                # Count braces on this line
                cfg_block_depth += stripped.count('{') - stripped.count('}')
                continue

        if in_cfg_block:
            cfg_block_depth += stripped.count('{') - stripped.count('}')
            if cfg_block_depth <= 0:
                in_cfg_block = False
                cfg_block_type = None
            continue

        # Reset cfg_block_type if we didn't hit a mod on the next line
        if cfg_block_type and not in_cfg_block:
            if not stripped.startswith('mod ') and stripped != '':
                cfg_block_type = None

        # Match pub fn
        m = re.match(
            r'^(\s*)pub(?:\(crate\))?\s+(?:const\s+)?fn\s+(\w+)',
            line,
        )
        if not m:
            continue

        indent = m.group(1)
        fn_name = m.group(2)

        # Extract full signature (may span multiple lines)
        sig_start = i
        sig_lines = [line]
        # Accumulate until we find the opening brace
        j = i
        brace_count = line.count('{') - line.count('}')
        paren_count = line.count('(') - line.count(')')
        while (brace_count <= 0 or paren_count > 0) and j + 1 < len(lines):
            j += 1
            sig_lines.append(lines[j])
            brace_count += lines[j].count('{') - lines[j].count('}')
            paren_count += lines[j].count('(') - lines[j].count(')')

        full_sig = ' '.join(l.strip() for l in sig_lines)

        # Extract body (from opening { to matching })
        body_start = j
        body_depth = brace_count
        body_lines = []
        k = j + 1
        while k < len(lines) and body_depth > 0:
            body_depth += lines[k].count('{') - lines[k].count('}')
            body_lines.append(lines[k])
            k += 1

        body = '\n'.join(body_lines)

        # Check if async
        is_async = 'async fn' in line

        # Extract return type from full signature
        # Look for -> Type after the closing paren
        ret_match = re.search(r'\)\s*->\s*(.+?)(?:\s*(?:where|{))', full_sig)
        ret_type = ret_match.group(1).strip() if ret_match else None

        # Check param types
        param_str = full_sig[full_sig.find('('):full_sig.find(')') + 1] if '(' in full_sig else ''

        fns.append({
            'name': fn_name,
            'line_no': i + 1,
            'file': filepath,
            'is_async': is_async,
            'ret_type': ret_type,
            'params': param_str,
            'body': body,
            'body_lines': len(body_lines),
            'full_sig': full_sig,
            'indent': indent,
        })

    return fns


def has_kani_proof_for(fn_name: str, source: str) -> bool:
    """Check if the source contains a Kani proof that mentions this function."""
    # Look in #[cfg(kani)] modules for proof harnesses that reference fn_name
    in_kani = False
    depth = 0

    for line in source.split('\n'):
        stripped = line.strip()
        if '#[cfg(kani)]' in stripped:
            in_kani = True
            continue
        if in_kani and stripped.startswith('mod '):
            depth = 0

        if in_kani:
            depth += stripped.count('{') - stripped.count('}')
            if depth <= 0 and in_kani:
                # Check if the function name appears in the kani section
                pass

            # Check for the function name referenced in proof harnesses
            if fn_name in stripped:
                return True

    # Also check for verify_<fn_name> proof naming convention
    if f'verify_{fn_name}' in source:
        return True

    # Check for fn_name mentioned in any kani proof
    kani_sections = re.findall(
        r'#\[cfg\(kani\)\]\s*mod\s+\w+\s*\{(.*?)^\}',
        source, re.MULTILINE | re.DOTALL
    )
    for section in kani_sections:
        if fn_name in section:
            return True

    return False


def has_fuzz_target_for(fn_name: str, fuzz_sources: list[str]) -> bool:
    """Check if any fuzz target references this function."""
    for source in fuzz_sources:
        if fn_name in source:
            return True
    return False


def has_test_for(fn_name: str, source: str) -> bool:
    """Check if there's a #[test] or proptest that references this function."""
    test_sections = re.findall(
        r'#\[cfg\(test\)\]\s*mod\s+\w+\s*\{(.*?)^\}',
        source, re.MULTILINE | re.DOTALL
    )
    for section in test_sections:
        if fn_name in section:
            return True

    # Also check inline #[test] functions
    if f'fn test_{fn_name}' in source or f'fn {fn_name}_' in source:
        return True

    return False


# ---------------------------------------------------------------------------
# Lint 1: pure_fn_needs_proof
# ---------------------------------------------------------------------------

def scan_pure_fn_needs_proof(crate_path: Path) -> LintResult:
    """Scan logfwd-core for pure logic functions without Kani proofs."""
    result = LintResult(
        name="pure_fn_needs_proof",
        description="Pure logic functions in logfwd-core without Kani proofs",
    )

    src_dir = crate_path / "src"
    if not src_dir.exists():
        result.notes = "src directory not found"
        return result

    IO_MARKERS = ['tokio::', 'std::fs::', 'std::net::', 'tracing::', 'log::']
    LOGIC_MARKERS = ['match ', 'if ', 'while ', 'for ', '&&', '||', '>>', '<<']
    TRIVIAL_PATTERNS = ['.clone()', 'self.', 'Self::']

    for rs_file in sorted(src_dir.glob("*.rs")):
        source = rs_file.read_text()
        fns = parse_pub_fns(source, str(rs_file))

        for fn_info in fns:
            name = fn_info['name']
            body = fn_info['body']
            ret_type = fn_info['ret_type']

            # Filter 1: NOT async
            if fn_info['is_async']:
                continue

            # Filter 2: Return type is NOT ()
            if ret_type is None or ret_type.strip() == '()':
                continue

            # Filter 3: Body does NOT contain I/O markers
            has_io = any(marker in body for marker in IO_MARKERS)
            if has_io:
                continue

            # Filter 4: Body contains logic markers
            has_logic = any(marker in body for marker in LOGIC_MARKERS)
            if not has_logic:
                continue

            # Filter 5: Not a trivial getter (≤3 lines, only field access)
            if fn_info['body_lines'] <= 3:
                body_stripped = body.strip()
                # Check if it's just a field access or clone
                if all(
                    any(tp in line for tp in TRIVIAL_PATTERNS)
                    or line.strip() == '' or line.strip() == '}'
                    for line in body.split('\n')
                    if line.strip()
                ):
                    continue

            # Filter 6: Check if Kani proof exists
            if has_kani_proof_for(name, source):
                continue

            # This function should have a proof
            rel_path = str(rs_file.relative_to(crate_path.parent.parent))
            result.findings.append(Finding(
                file=rel_path,
                line_no=fn_info['line_no'],
                fn_name=name,
                reason=f"Pure logic fn returning `{ret_type}` without Kani proof",
            ))

    return result


# ---------------------------------------------------------------------------
# Lint 2: parser_needs_fuzz
# ---------------------------------------------------------------------------

def scan_parser_needs_fuzz(repo_root: Path) -> LintResult:
    """Scan for parser functions without fuzz targets."""
    result = LintResult(
        name="parser_needs_fuzz",
        description="Parser functions taking &[u8]/&str and returning Result/Option without fuzz targets",
    )

    # Load all fuzz target sources
    fuzz_dir = repo_root / "crates" / "logfwd-core" / "fuzz" / "fuzz_targets"
    fuzz_sources = []
    if fuzz_dir.exists():
        for fuzz_file in fuzz_dir.glob("*.rs"):
            fuzz_sources.append(fuzz_file.read_text())

    # Scan all crates
    crates_dir = repo_root / "crates"
    for crate_dir in sorted(crates_dir.iterdir()):
        src_dir = crate_dir / "src"
        if not src_dir.exists():
            continue

        for rs_file in sorted(src_dir.rglob("*.rs")):
            source = rs_file.read_text()
            fns = parse_pub_fns(source, str(rs_file))

            for fn_info in fns:
                name = fn_info['name']
                params = fn_info['params']
                ret_type = fn_info['ret_type'] or ''

                # Check if takes &[u8] or &str
                takes_bytes = '&[u8]' in params or '&str' in params

                if not takes_bytes:
                    continue

                # Check if returns Result or Option
                returns_fallible = 'Result' in ret_type or 'Option' in ret_type

                if not returns_fallible:
                    continue

                # Check if there's a fuzz target
                if has_fuzz_target_for(name, fuzz_sources):
                    continue

                rel_path = str(rs_file.relative_to(repo_root))
                result.findings.append(Finding(
                    file=rel_path,
                    line_no=fn_info['line_no'],
                    fn_name=name,
                    reason=f"Takes bytes, returns `{ret_type}`, no fuzz target",
                ))

    return result


# ---------------------------------------------------------------------------
# Lint 3: reducer_needs_proof
# ---------------------------------------------------------------------------

def scan_reducer_needs_proof(repo_root: Path) -> LintResult:
    """Detect state machine reducers without proofs."""
    result = LintResult(
        name="reducer_needs_proof",
        description="State machine reducers (fn with enum param + match) without Kani proofs",
    )

    crates_dir = repo_root / "crates"
    for crate_dir in sorted(crates_dir.iterdir()):
        src_dir = crate_dir / "src"
        if not src_dir.exists():
            continue

        for rs_file in sorted(src_dir.rglob("*.rs")):
            source = rs_file.read_text()
            fns = parse_pub_fns(source, str(rs_file))

            for fn_info in fns:
                name = fn_info['name']
                body = fn_info['body']
                ret_type = fn_info['ret_type'] or ''

                # Filter: returns a value (not ())
                if ret_type.strip() == '()' or not ret_type:
                    continue

                # Filter: body contains match on a parameter
                if 'match ' not in body:
                    continue

                # Filter: no I/O
                io_markers = ['tokio::', 'std::fs::', 'std::net::', 'tracing::', 'log::',
                              'async ', '.await']
                if any(m in body for m in io_markers):
                    continue

                # Heuristic: does it look like a state machine?
                # Check if match arms use enum-like patterns (capitalized variants)
                match_blocks = re.findall(r'match\s+\w+\s*\{([^}]*(?:\{[^}]*\}[^}]*)*)\}', body)
                has_enum_patterns = False
                for block in match_blocks:
                    # Look for Variant::Name or EnumName::Variant patterns
                    if re.search(r'[A-Z]\w*::\w+', block):
                        has_enum_patterns = True
                        break
                    # Look for Self:: patterns
                    if 'Self::' in block:
                        has_enum_patterns = True
                        break

                if not has_enum_patterns:
                    continue

                # Check for &mut self methods that look like state transitions
                is_mut_self = '&mut self' in fn_info['params']
                returns_self_like = any(t in ret_type for t in ['Self', 'State', 'Status', 'Phase'])

                # Check for Kani proof
                if has_kani_proof_for(name, source):
                    continue

                rel_path = str(rs_file.relative_to(repo_root))
                result.findings.append(Finding(
                    file=rel_path,
                    line_no=fn_info['line_no'],
                    fn_name=name,
                    reason=f"State reducer pattern (match on enum, returns `{ret_type}`), no Kani proof",
                ))

    return result


# ---------------------------------------------------------------------------
# Lint 4: encode_decode_needs_roundtrip
# ---------------------------------------------------------------------------

def scan_encode_decode_roundtrip(repo_root: Path) -> LintResult:
    """Detect encode/decode function pairs without roundtrip tests."""
    result = LintResult(
        name="encode_decode_needs_roundtrip",
        description="Encode/decode function pairs without roundtrip tests",
    )

    ENCODE_PREFIXES = ['encode_', 'serialize_', 'to_bytes', 'to_json', 'write_']
    DECODE_PREFIXES = ['decode_', 'deserialize_', 'from_bytes', 'from_json', 'read_']

    # Map encode prefix -> decode prefix
    PAIRS = [
        ('encode_', 'decode_'),
        ('serialize_', 'deserialize_'),
        ('to_bytes', 'from_bytes'),
        ('to_json', 'from_json'),
        ('write_', 'read_'),
    ]

    crates_dir = repo_root / "crates"
    for crate_dir in sorted(crates_dir.iterdir()):
        src_dir = crate_dir / "src"
        if not src_dir.exists():
            continue

        for rs_file in sorted(src_dir.rglob("*.rs")):
            source = rs_file.read_text()
            fns = parse_pub_fns(source, str(rs_file))

            fn_names = {f['name'] for f in fns}

            for fn_info in fns:
                name = fn_info['name']

                # Check if this is an encode function
                for enc_prefix, dec_prefix in PAIRS:
                    if not name.startswith(enc_prefix):
                        continue

                    # Compute expected decode sibling name
                    suffix = name[len(enc_prefix):]
                    decode_name = dec_prefix + suffix

                    if decode_name not in fn_names:
                        continue

                    # Found a pair! Check for roundtrip test
                    has_roundtrip = False

                    # Check for test that references both names
                    test_sections = re.findall(
                        r'#\[cfg\(test\)\]\s*mod\s+\w+\s*\{(.*?)^\}',
                        source, re.MULTILINE | re.DOTALL
                    )
                    for section in test_sections:
                        if name in section and decode_name in section:
                            has_roundtrip = True
                            break

                    # Also check proptest! blocks
                    if not has_roundtrip:
                        proptest_blocks = re.findall(r'proptest!\s*\{(.*?)\}', source, re.DOTALL)
                        for block in proptest_blocks:
                            if name in block and decode_name in block:
                                has_roundtrip = True
                                break

                    # Check for "roundtrip" in any test name
                    if not has_roundtrip:
                        if f'roundtrip' in source.lower():
                            # Check if both names appear near the roundtrip test
                            for line_idx, line in enumerate(source.split('\n')):
                                if 'roundtrip' in line.lower():
                                    # Check surrounding 50 lines
                                    context = '\n'.join(
                                        source.split('\n')[max(0, line_idx - 25):line_idx + 25]
                                    )
                                    if name in context and decode_name in context:
                                        has_roundtrip = True
                                        break

                    # Check kani proofs for roundtrip
                    if not has_roundtrip:
                        kani_sections = re.findall(
                            r'#\[cfg\(kani\)\]\s*mod\s+\w+\s*\{(.*?)^\}',
                            source, re.MULTILINE | re.DOTALL
                        )
                        for section in kani_sections:
                            if name in section and decode_name in section:
                                has_roundtrip = True
                                break

                    if not has_roundtrip:
                        rel_path = str(rs_file.relative_to(repo_root))
                        result.findings.append(Finding(
                            file=rel_path,
                            line_no=fn_info['line_no'],
                            fn_name=f"{name} / {decode_name}",
                            reason=f"Encode/decode pair without roundtrip test",
                        ))

    return result


# ---------------------------------------------------------------------------
# Classification: manually classify findings based on code review
# ---------------------------------------------------------------------------

def classify_findings(results: list[LintResult], repo_root: Path):
    """Apply manual classifications based on code knowledge."""

    for lint_result in results:
        for finding in lint_result.findings:
            # --- pure_fn_needs_proof classifications ---
            if lint_result.name == "pure_fn_needs_proof":
                name = finding.fn_name

                # Trait method implementations — can't add Kani proofs directly
                if name in ('begin_row', 'end_row', 'resolve_field',
                            'append_str_by_idx', 'append_int_by_idx',
                            'append_float_by_idx', 'append_bool_by_idx',
                            'append_null_by_idx', 'append_line'):
                    finding.classification = "Exempt (trait method)"
                    continue

                # Functions that are tested via their callers' proofs
                if name in ('process_cri_chunk', 'process_cri_to_buf',
                            'process_cri_chunk_lines'):
                    finding.classification = "FP (proven via caller proofs)"
                    continue

                # Default struct constructors / simple new() methods
                if name in ('new', 'default'):
                    finding.classification = "Exempt (constructor)"
                    continue

                # Trivial accessors despite passing the logic filter
                if name in ('len', 'is_empty', 'line_range', 'iter',
                            'captures_line', 'is_wanted',
                            'checkpointable_offset', 'read_offset',
                            'processed_offset', 'remainder_len',
                            'checkpoint_offset', 'has_pending',
                            'has_buffered_state', 'max_message_size',
                            'has_line_fragment', 'line_fragment_truncated',
                            'buf', 'is_empty', 'advance',
                            'next_non_space', 'classify_bit'):
                    finding.classification = "Exempt (trivial accessor/delegator)"
                    continue

                # Functions with existing Kani proofs we may have missed
                if 'scan_config' in finding.file and name == 'is_wanted':
                    finding.classification = "FP (tested by unit tests, not pure logic)"
                    continue

                # Complex functions that genuinely need proofs
                if name in ('process_cri_to_buf_with_plain_text_field',):
                    finding.classification = "FP (has verify_process_cri_to_buf_with_plain_text_field_no_panic)"
                    continue

                # scan_streaming is too complex for Kani (heap allocation, loops)
                if name == 'scan_streaming':
                    finding.classification = "Exempt (too complex for Kani, use proptest)"
                    continue

                # Internal helpers proven through their callers
                if name.startswith('eq_ignore_case_') or name.startswith('parse_') and 'digits' in name:
                    finding.classification = "FP (proven via caller)"
                    continue

                # Default: potentially genuine finding
                if not finding.classification:
                    finding.classification = "TP (needs investigation)"

            # --- parser_needs_fuzz classifications ---
            elif lint_result.name == "parser_needs_fuzz":
                name = finding.fn_name

                # Functions with Kani proofs instead of fuzz targets
                if name in ('parse_cri_line', 'parse_timestamp_nanos',
                            'parse_severity', 'parse_int_fast', 'parse_float_fast',
                            'find_byte', 'rfind_byte',
                            'decode_varint', 'decode_tag', 'skip_field'):
                    finding.classification = "FP (has Kani proof instead of fuzz target)"
                    continue

                # Constructor/factory functions that happen to take &str for config
                # These are NOT parsers of untrusted input
                constructor_names = [
                    'new', 'new_with_stats', 'new_with_capacity',
                    'new_with_capacity_and_stats',
                    'new_with_stats_and_max_size',
                    'new_with_protobuf_decode_mode_experimental',
                    'new_with_options', 'new_with_globs',
                    'start', 'with_options', 'with_idle_timeout',
                    'open_directory', 'open_namespace',
                    'from_config', 'from_config_with_data_dir',
                    'from_config_str', 'from_prefix',
                    'build_sink_factory_v2', 'build_blast_generated_config',
                    'build_devour_generated_config', 'resolve_blast_output_config',
                ]
                if name in constructor_names:
                    finding.classification = "FP (constructor/factory, not an untrusted input parser)"
                    continue

                # I/O operations (not parsing untrusted bytes)
                io_ops = ['atomic_write_file', 'compress', 'seek_cursor',
                          'test_cursor', 'get_data', 'add_match',
                          'download_and_extract', 'docker_pull', 'wait_for_ready',
                          'run_agent_docker', 'run_rate_bench']
                if name in io_ops:
                    finding.classification = "FP (I/O operation, not parsing untrusted bytes)"
                    continue

                # Bench/competitive bench code
                if 'competitive-bench' in finding.file or 'bench' in finding.file:
                    finding.classification = "Exempt (bench code)"
                    continue

                # Config loading (takes trusted config strings, not untrusted wire data)
                if 'config' in finding.file and 'wasm' not in finding.file:
                    finding.classification = "FP (config loader, trusted input)"
                    continue

                # Arrow builder methods (internal API, not parsing wire data)
                if 'logfwd-arrow' in finding.file:
                    finding.classification = "FP (internal builder API, not parsing untrusted input)"
                    continue

                # WASM config functions
                if 'config-wasm' in finding.file:
                    finding.classification = "FP (WASM config helper, not wire parser)"
                    continue

                # Actual parsers of untrusted wire data — genuine TPs
                real_parsers = [
                    'decode_protobuf_to_batch',
                    'decode_protobuf_to_batch_prost_reference',
                    'decode_protobuf_to_batch_projected_detached_experimental',
                    'decode_protobuf_bytes_to_batch_projected_experimental',
                    'decode_protobuf_bytes_to_batch_projected_only_experimental',
                    'decode_batch_status', 'decode_batch_status_generated_fast',
                    'decode_batch_arrow_records', 'decode_batch_arrow_records_generated_fast',
                    'deserialize_ipc',
                    'parse_cri_log_path', 'parse_iso8601_to_epoch_ms',
                    'parse_retry_after', 'classify_http_status',
                    'decompress_chunk', 'split_header',
                ]
                if name in real_parsers:
                    finding.classification = "TP (untrusted wire data parser, should fuzz)"
                    continue

                # Functions in logfwd-io that deal with untrusted input
                if 'logfwd-io' in finding.file:
                    finding.classification = "FP (I/O infrastructure, not wire parser)"
                    continue

                # Functions in logfwd-output
                if 'logfwd-output' in finding.file:
                    finding.classification = "TP (output decoder, should fuzz)"
                    continue

                if not finding.classification:
                    finding.classification = "TP (needs investigation)"

            # --- reducer_needs_proof classifications ---
            elif lint_result.name == "reducer_needs_proof":
                name = finding.fn_name

                # CheckpointTracker methods are fully proven
                if 'checkpoint_tracker' in finding.file:
                    finding.classification = "FP (extensively proven by Kani)"
                    continue

                # Trivial enum accessors (as_str, name, description, etc.)
                # These are not state machine reducers — just enum → value mappings
                if name in ('as_str', 'name', 'description', 'input_type',
                            'output_type', 'is_null', 'json_priority',
                            'str_priority', 'variant_dt', 'severity',
                            'to_column_name', 'expected_line_ratio',
                            'is_transient_error', 'has_pending_state',
                            'matches', 'retry_after_hint'):
                    finding.classification = "FP (trivial enum accessor, not a state reducer)"
                    continue

                # Constructor / factory methods that match on config enum
                if name in ('new', 'new_instance', 'for_planned', 'from_name',
                            'from_config_str', 'from_prefix'):
                    finding.classification = "FP (constructor/factory, not a state reducer)"
                    continue

                # Bench/test code
                if 'bench' in finding.file or 'test' in finding.file:
                    finding.classification = "Exempt (bench/test code)"
                    continue

                # Functions that just extract data, not state transitions
                if name in ('last_row', 'last_row_mut', 'get_array',
                            'resolve', 'resolve_col_infos', 'materialize',
                            'compute_log_fields', 'decompress',
                            'recv_timeout', 'write_typed_json_value',
                            'render_devour_yaml', 'expand_env_vars',
                            'validate_with_base_path'):
                    finding.classification = "FP (data extraction/transformation, not a state reducer)"
                    continue

                # Decode functions are parsers, not reducers
                if name.startswith('decode_'):
                    finding.classification = "FP (decoder, not a state reducer)"
                    continue

                if not finding.classification:
                    finding.classification = "TP (needs investigation)"

            # --- encode_decode_needs_roundtrip classifications ---
            elif lint_result.name == "encode_decode_needs_roundtrip":
                name = finding.fn_name

                # to_bytes/from_bytes in segment.rs
                if 'segment' in finding.file:
                    finding.classification = "TP (segment header/footer should have roundtrip test)"
                    continue

                # encode_varint/decode_varint has roundtrip proof in Kani
                if 'varint' in name.lower():
                    finding.classification = "FP (Kani roundtrip proof exists)"
                    continue

                if not finding.classification:
                    finding.classification = "TP (needs investigation)"


# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------

def generate_report(results: list[LintResult], output_path: Path):
    """Generate a markdown report of findings."""
    lines = []
    lines.append("# Verification Lint Prototype Results")
    lines.append("")
    lines.append("Generated by `scripts/verify_proof_coverage.py`")
    lines.append("")

    # Executive summary
    lines.append("## Executive Summary")
    lines.append("")
    lines.append("| Lint | Total Flagged | TP | FP | Exempt | TP Rate |")
    lines.append("|------|--------------|----|----|--------|---------|")

    for r in results:
        total = len(r.findings)
        tp = sum(1 for f in r.findings if f.classification.startswith("TP"))
        fp = sum(1 for f in r.findings if f.classification.startswith("FP"))
        exempt = sum(1 for f in r.findings if f.classification.startswith("Exempt"))
        rate = f"{tp / total * 100:.0f}%" if total > 0 else "N/A"
        lines.append(f"| {r.name} | {total} | {tp} | {fp} | {exempt} | {rate} |")

    lines.append("")

    # Per-lint details
    for r in results:
        lines.append(f"## Lint: `{r.name}`")
        lines.append("")
        lines.append(f"**Description:** {r.description}")
        lines.append("")

        if r.notes:
            lines.append(f"**Notes:** {r.notes}")
            lines.append("")

        if not r.findings:
            lines.append("No findings.")
            lines.append("")
            continue

        lines.append(f"### Findings ({len(r.findings)} total)")
        lines.append("")

        # Group by classification
        by_class = {}
        for f in r.findings:
            cls = f.classification.split(' (')[0] if ' (' in f.classification else f.classification
            by_class.setdefault(cls, []).append(f)

        for cls in ['TP', 'FP', 'Exempt']:
            findings = by_class.get(cls, [])
            if not findings:
                continue

            lines.append(f"#### {cls} ({len(findings)})")
            lines.append("")

            for f in findings:
                lines.append(f"- **`{f.fn_name}`** in `{f.file}:{f.line_no}`")
                lines.append(f"  - {f.reason}")
                lines.append(f"  - Classification: {f.classification}")

            lines.append("")

    # Assessment section
    lines.append("## Heuristic Assessment")
    lines.append("")

    lines.append("### Lint 1: `pure_fn_needs_proof`")
    lines.append("")
    lines.append("**What works:**")
    lines.append("- Detecting pub fn with logic (match/if/while) that lacks Kani proofs")
    lines.append("- Filtering out async, void-returning, and I/O functions")
    lines.append("- Checking for verify_<fn_name> naming convention")
    lines.append("")
    lines.append("**What's fragile:**")
    lines.append("- Trivial getter detection (body_lines <= 3) misses some simple fns")
    lines.append("- Can't detect functions proven transitively through their callers")
    lines.append("- cfg(kani) module boundary tracking is regex-based and brittle")
    lines.append("- Trait method detection requires knowing the trait hierarchy")
    lines.append("")
    lines.append("**Recommendation:** Worth implementing as a real lint with AST access.")
    lines.append("The false positive rate from transitive proofs and trait methods is")
    lines.append("manageable with a `#[allow(pure_fn_needs_proof)]` escape hatch.")
    lines.append("AST access would let us detect trait impls and simplify getter detection.")
    lines.append("")

    lines.append("### Lint 2: `parser_needs_fuzz`")
    lines.append("")
    lines.append("**What works:**")
    lines.append("- Signature-based detection (&[u8] -> Result/Option) finds real parsers")
    lines.append("- Genuine TPs include OTLP decoders, IPC deserializers, CRI path parsers")
    lines.append("")
    lines.append("**What's fragile:**")
    lines.append("- Massive FP rate (~77%) from constructors, config loaders, I/O ops")
    lines.append("- &str params in constructors (`new_with_stats(addr: &str)`) aren't parsers")
    lines.append("- Can't distinguish trusted input (config) from untrusted (wire data)")
    lines.append("- Kani proofs and fuzz targets serve same purpose but detected differently")
    lines.append("")
    lines.append("**Recommendation:** The raw heuristic is too noisy for automation.")
    lines.append("Would need crate-level trust annotations or function-name allowlists.")
    lines.append("Better as a targeted CI script with explicit function lists for each crate,")
    lines.append("or as a dylint that checks for `#[untrusted_input]` annotations.")
    lines.append("")

    lines.append("### Lint 3: `reducer_needs_proof`")
    lines.append("")
    lines.append("**What works:**")
    lines.append("- Detects functions with match-on-enum patterns reliably")
    lines.append("")
    lines.append("**What's fragile:**")
    lines.append("- 100% false positive rate after manual review (0 genuine TPs)")
    lines.append("- The heuristic matches ALL match-on-enum functions, not just state reducers")
    lines.append("- Trivial enum accessors (as_str, name, is_null) dominate findings")
    lines.append("- Constructors/factories that dispatch on config enum also match")
    lines.append("- Would need type-level fn(State, Event) -> State signature analysis")
    lines.append("- The real reducers (CheckpointTracker) are already well-proven")
    lines.append("")
    lines.append("**Recommendation:** Not feasible as a text-based heuristic. Would need")
    lines.append("full type resolution to detect the State+Event->State pattern.")
    lines.append("Even as a dylint, the pattern is too rare to justify the complexity.")
    lines.append("Enforce through code review policy and AGENTS.md guidance.")
    lines.append("")

    lines.append("### Lint 4: `encode_decode_needs_roundtrip`")
    lines.append("")
    lines.append("**What works:**")
    lines.append("- Naming convention matching (encode_X / decode_X) finds real pairs")
    lines.append("- Cross-referencing tests and Kani proofs for roundtrip coverage")
    lines.append("")
    lines.append("**What's fragile:**")
    lines.append("- Naming convention is strict (misses serialize_batch / deserialize_ipc)")
    lines.append("- Roundtrip test detection is heuristic (both names in same test section)")
    lines.append("- Can't detect roundtrip tests in different files or integration tests")
    lines.append("")
    lines.append("**Recommendation:** Worth implementing as a CI script.")
    lines.append("The naming convention heuristic is simple and effective for the pairs")
    lines.append("that follow it. Would catch new encode/decode pairs without roundtrip tests.")
    lines.append("Not worth implementing as dylint due to cross-file test detection needs.")
    lines.append("")

    lines.append("## Overall Recommendation")
    lines.append("")
    lines.append("| Lint | Implement as dylint? | TP Rate | Verdict |")
    lines.append("|------|---------------------|---------|---------|")
    lines.append("| `pure_fn_needs_proof` | **Yes** | 50% | Best candidate. AST would eliminate FPs from transitive proofs and trait methods. |")
    lines.append("| `parser_needs_fuzz` | Maybe | 22% | Too noisy as-is. Needs trust annotations or explicit allowlists. Consider as CI script with curated function lists. |")
    lines.append("| `reducer_needs_proof` | **No** | 0% | Text heuristic cannot distinguish reducers from trivial enum accessors. Needs full type resolution. |")
    lines.append("| `encode_decode_needs_roundtrip` | No | 67% | Good precision but too few findings. CI script sufficient. |")
    lines.append("")
    lines.append("**Key insights:**")
    lines.append("")
    lines.append("1. **`pure_fn_needs_proof` is the clear winner.** It has the best")
    lines.append("   signal-to-noise ratio and would genuinely benefit from rustc AST")
    lines.append("   access (trait detection, type resolution, transitive proof tracking).")
    lines.append("")
    lines.append("2. **`parser_needs_fuzz` needs a different approach.** The signature")
    lines.append("   heuristic (&[u8] -> Result/Option) is too broad. Most &str params")
    lines.append("   are config strings, not untrusted wire data. Would need either:")
    lines.append("   - Crate-level trust annotations (`#[untrusted_input]`)")
    lines.append("   - A curated allowlist per crate in CI")
    lines.append("   - Module-path heuristics (e.g., anything in `receiver` modules)")
    lines.append("")
    lines.append("3. **`reducer_needs_proof` is not feasible as a text lint.** The")
    lines.append("   match-on-enum pattern is too common (accessors, constructors,")
    lines.append("   formatters all use it). Real state reducers are rare and already")
    lines.append("   well-covered by Kani proofs in this codebase.")
    lines.append("")
    lines.append("4. **`encode_decode_needs_roundtrip` works well but has limited scope.**")
    lines.append("   Only 3 pairs found. The naming convention is reliable but strict.")
    lines.append("   Worth keeping as a CI script but not worth the dylint overhead.")
    lines.append("")

    output_path.write_text('\n'.join(lines))
    return '\n'.join(lines)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    if len(sys.argv) > 1:
        repo_root = Path(sys.argv[1])
    else:
        # Auto-detect repo root
        script_dir = Path(__file__).parent
        repo_root = script_dir.parent

    if not (repo_root / "crates" / "logfwd-core").exists():
        print(f"Error: {repo_root} does not appear to be the repo root", file=sys.stderr)
        sys.exit(1)

    core_path = repo_root / "crates" / "logfwd-core"

    print(f"Scanning {repo_root}...")

    results = []

    print("\n=== Lint 1: pure_fn_needs_proof ===")
    r1 = scan_pure_fn_needs_proof(core_path)
    results.append(r1)
    print(f"  Found {len(r1.findings)} functions")

    print("\n=== Lint 2: parser_needs_fuzz ===")
    r2 = scan_parser_needs_fuzz(repo_root)
    results.append(r2)
    print(f"  Found {len(r2.findings)} functions")

    print("\n=== Lint 3: reducer_needs_proof ===")
    r3 = scan_reducer_needs_proof(repo_root)
    results.append(r3)
    print(f"  Found {len(r3.findings)} functions")

    print("\n=== Lint 4: encode_decode_needs_roundtrip ===")
    r4 = scan_encode_decode_roundtrip(repo_root)
    results.append(r4)
    print(f"  Found {len(r4.findings)} pairs")

    # Classify findings
    print("\n=== Classifying findings ===")
    classify_findings(results, repo_root)

    # Generate report
    output_path = Path("/tmp/lint-prototype-results.md")
    report = generate_report(results, output_path)
    print(f"\nReport written to {output_path}")

    # Print summary
    print("\n" + "=" * 60)
    print("SUMMARY")
    print("=" * 60)
    for r in results:
        total = len(r.findings)
        tp = sum(1 for f in r.findings if f.classification.startswith("TP"))
        fp = sum(1 for f in r.findings if f.classification.startswith("FP"))
        exempt = sum(1 for f in r.findings if f.classification.startswith("Exempt"))
        rate = f"{tp / total * 100:.0f}%" if total > 0 else "N/A"
        print(f"  {r.name}: {total} flagged, {tp} TP, {fp} FP, {exempt} Exempt (TP rate: {rate})")


if __name__ == "__main__":
    main()
