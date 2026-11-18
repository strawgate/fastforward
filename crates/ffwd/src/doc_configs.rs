//! Guards the YAML config snippets in the README and the user-facing book.
//!
//! Every fenced `yaml` block is classified by its top-level keys:
//!
//! - `pipelines` → a full config; it must load and pass read-only validation.
//! - only `output`/`outputs` (plus `server`/`storage`) → an `ff send`
//!   destination config; it must load after the `ff send` stdin rewrite.
//! - only `transform`/`enrichment` → a pipeline-field fragment; not validated.
//! - top-level `input`/`inputs` without `pipelines` → the removed legacy
//!   single-pipeline shape; always rejected.
//!
//! Kubernetes `ConfigMap` documents are checked the same way for each
//! `*.yaml`/`*.yml` data entry. Anything else (collector configs, manifests)
//! is ignored.

use std::path::{Path, PathBuf};

use serde_yaml_ng::Value;

use crate::send::build_stdin_send_config_yaml;
use crate::validate::validate_pipelines_read_only;

const SEND_DESTINATION_KEYS: &[&str] = &["output", "outputs", "server", "storage"];
const PIPELINE_FRAGMENT_KEYS: &[&str] = &["transform", "enrichment"];
const LEGACY_INPUT_KEYS: &[&str] = &["input", "inputs"];

/// A fenced YAML block and the 1-based line of its opening fence.
struct YamlBlock {
    line: usize,
    body: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collect_doc_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_doc_files(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|ext| ext == "md" || ext == "mdx")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn extract_yaml_blocks(markdown: &str) -> Vec<YamlBlock> {
    let mut blocks = Vec::new();
    let mut lines = markdown.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        let trimmed = line.trim_start();
        let Some(info) = trimmed.strip_prefix("```") else {
            continue;
        };
        let indent = line.len() - trimmed.len();
        let is_yaml = matches!(info.split_whitespace().next(), Some("yaml" | "yml"));
        let mut body = String::new();
        for (_, inner) in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                break;
            }
            let dedented = inner.get(indent..).filter(|_| inner.len() >= indent);
            body.push_str(dedented.unwrap_or_else(|| inner.trim_start()));
            body.push('\n');
        }
        if is_yaml {
            blocks.push(YamlBlock {
                line: index + 1,
                body,
            });
        }
    }
    blocks
}

/// Replace `${VAR}` placeholders with a value that fits the field: a URL for
/// endpoints, a number for `workers`, and a plain string everywhere else.
fn replace_env_placeholders(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.split_inclusive('\n') {
        let key = line.trim_start().trim_start_matches("- ");
        let replacement = if key.starts_with("endpoint:") || key.starts_with("metrics_endpoint:") {
            "http://example-env-value:4318"
        } else if key.starts_with("workers:") {
            "2"
        } else {
            "example-env-value"
        };
        let mut rest = line;
        while let Some(start) = rest.find("${") {
            let Some(len) = rest[start..].find('}') else {
                break;
            };
            out.push_str(&rest[..start]);
            out.push_str(replacement);
            rest = &rest[start + len + 1..];
        }
        out.push_str(rest);
    }
    out
}

fn top_level_keys(value: &Value) -> Vec<String> {
    value
        .as_mapping()
        .map(|mapping| {
            mapping
                .keys()
                .filter_map(|key| key.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Read-only validation errors that are limits of the validator, not of the
/// documented config. `Config::load_str` has already accepted the config.
fn is_known_read_only_gap(err: &str) -> bool {
    // Some inputs (S3, macOS log) cannot be checked read-only.
    err.contains("not yet supported in read-only validation")
        // Read-only SQL planning adds source-metadata columns twice, so a join
        // on `l."file.path"` fails here even though `ff run` accepts it.
        || err.contains(r#"duplicate qualified field name logs."file.path""#)
}

/// Run read-only pipeline validation, keeping each pipeline's error message.
fn validate_read_only(config: &ffwd_config::Config) -> Result<(), String> {
    // Lite builds (no `datafusion`) only accept `SELECT * FROM logs` and no
    // enrichment, so semantic checks there would reject valid doc SQL. The
    // config has already loaded, which is the format check this test is for.
    if cfg!(not(feature = "datafusion")) {
        return Ok(());
    }
    let mut errors = Vec::new();
    match validate_pipelines_read_only(config, None, |_| {}, |err| errors.push(err)) {
        Ok(()) => Ok(()),
        Err(err) if errors.is_empty() => Err(err.to_string()),
        Err(_) => {
            errors.retain(|err| !is_known_read_only_gap(err));
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }
    }
}

/// Check one YAML document. Returns `Err` with a reason when it is a config
/// that does not load under the current format.
fn check_config_document(raw: &str, value: &Value) -> Result<(), String> {
    let keys = top_level_keys(value);
    let has = |name: &str| keys.iter().any(|key| key == name);
    let only = |allowed: &[&str]| keys.iter().all(|key| allowed.contains(&key.as_str()));

    if has("pipelines") {
        let config = ffwd_config::Config::load_str(raw).map_err(|e| e.to_string())?;
        return validate_read_only(&config);
    }
    if let Some(key) = LEGACY_INPUT_KEYS.iter().find(|key| has(key)) {
        return Err(format!(
            "top-level `{key}` is the removed single-pipeline format; nest it under `pipelines.<name>.inputs`"
        ));
    }
    if (has("output") || has("outputs")) && only(SEND_DESTINATION_KEYS) {
        let yaml = build_stdin_send_config_yaml(raw, None, None, &[]).map_err(|e| e.to_string())?;
        let config = ffwd_config::Config::load_str(&yaml).map_err(|e| e.to_string())?;
        return validate_read_only(&config);
    }
    if keys
        .iter()
        .any(|key| key == "output" || key == "outputs" || key == "transform")
        && !only(PIPELINE_FRAGMENT_KEYS)
    {
        return Err(format!(
            "mixes pipeline fields with other top-level keys {keys:?}; use a full `pipelines` config"
        ));
    }
    Ok(())
}

/// Check every YAML document in a fenced block, including ConfigMap entries.
fn check_yaml_block(body: &str) -> Vec<String> {
    let body = replace_env_placeholders(body);
    let mut failures = Vec::new();
    // Multi-document blocks (Kubernetes manifests) are split on `---` lines.
    for document in body.split("\n---\n") {
        if document.trim().is_empty() {
            continue;
        }
        let value = match serde_yaml_ng::from_str::<Value>(document) {
            Ok(value) => value,
            Err(err) => {
                failures.push(format!("invalid YAML: {err}"));
                break;
            }
        };
        if value.get("kind").and_then(Value::as_str) == Some("ConfigMap") {
            let Some(data) = value.get("data").and_then(Value::as_mapping) else {
                continue;
            };
            for (key, entry) in data {
                let (Some(key), Some(text)) = (key.as_str(), entry.as_str()) else {
                    continue;
                };
                let is_yaml_entry = Path::new(key).extension().is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
                });
                if !is_yaml_entry {
                    continue;
                }
                match serde_yaml_ng::from_str::<Value>(text) {
                    Ok(inner) => {
                        if let Err(err) = check_config_document(text, &inner) {
                            failures.push(format!("ConfigMap entry `{key}`: {err}"));
                        }
                    }
                    Err(err) => {
                        failures.push(format!("ConfigMap entry `{key}`: invalid YAML: {err}"));
                    }
                }
            }
            continue;
        }
        let raw = serde_yaml_ng::to_string(&value).unwrap_or_default();
        if let Err(err) = check_config_document(&raw, &value) {
            failures.push(err);
        }
    }
    failures
}

#[test]
fn doc_yaml_configs_use_current_config_format() {
    let root = repo_root();
    let mut files = vec![root.join("README.md")];
    collect_doc_files(&root.join("book/src/content/docs"), &mut files)
        .expect("should collect book docs");
    files.sort();

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for path in &files {
        let markdown = std::fs::read_to_string(path).expect("should read doc file");
        let display = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string();
        for block in extract_yaml_blocks(&markdown) {
            checked += 1;
            for failure in check_yaml_block(&block.body) {
                failures.push(format!("{display}:{}: {failure}", block.line));
            }
        }
    }

    assert!(
        checked > 0,
        "expected YAML blocks in README.md and book docs"
    );
    assert!(
        failures.is_empty(),
        "doc YAML configs must load with the current config format:\n{}",
        failures.join("\n")
    );
}

#[test]
fn legacy_top_level_input_is_rejected() {
    let failures = check_yaml_block("input:\n  type: stdin\noutput:\n  type: stdout\n");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("removed single-pipeline format"));
}

#[test]
fn legacy_config_inside_configmap_is_rejected() {
    let manifest = "apiVersion: v1\nkind: ConfigMap\ndata:\n  config.yaml: |\n    input:\n      type: stdin\n    output:\n      type: stdout\n";
    let failures = check_yaml_block(manifest);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].starts_with("ConfigMap entry `config.yaml`"));
}

#[test]
fn send_destination_and_transform_fragments_are_accepted() {
    assert!(check_yaml_block("output:\n  type: stdout\n").is_empty());
    assert!(check_yaml_block("transform: SELECT * FROM logs\n").is_empty());
}

#[test]
fn yaml_blocks_are_extracted_with_indentation_removed() {
    let markdown = "text\n  ```yaml\n  a: 1\n  ```\n```bash\necho hi\n```\n";
    let blocks = extract_yaml_blocks(markdown);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].line, 2);
    assert_eq!(blocks[0].body, "a: 1\n");
}
