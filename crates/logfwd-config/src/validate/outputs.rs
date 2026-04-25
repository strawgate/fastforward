use crate::types::{
    CompressionFormat, ConfigError, ElasticsearchRequestMode, Format, OutputConfigV2, OutputType,
};
use std::collections::HashMap;

use super::common::validation_message;
use super::endpoints::{validate_endpoint_url, validate_host_port};

fn validate_url_output_endpoint(
    pipeline_name: &str,
    label: &str,
    output_type: OutputType,
    endpoint: Option<&str>,
) -> Result<(), ConfigError> {
    let Some(endpoint) = endpoint else {
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': {output_type} output requires 'endpoint'",
        )));
    };

    if let Err(err) = validate_endpoint_url(endpoint) {
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': {}",
            validation_message(err)
        )));
    }

    Ok(())
}

fn validate_socket_output_endpoint(
    pipeline_name: &str,
    label: &str,
    output_type: OutputType,
    endpoint: Option<&str>,
) -> Result<(), ConfigError> {
    let Some(endpoint) = endpoint else {
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': {output_type} output requires 'endpoint'",
        )));
    };

    if let Err(err) = validate_host_port(endpoint) {
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': {}",
            validation_message(err)
        )));
    }

    Ok(())
}

fn validate_elasticsearch_index(
    pipeline_name: &str,
    label: &str,
    index: Option<&str>,
) -> Result<(), ConfigError> {
    if let Some(index) = index
        && index.trim().is_empty()
    {
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': elasticsearch 'index' must not be empty"
        )));
    }
    if let Some(index) = index
        && let Some(bad) = es_illegal_index_char(index)
    {
        let reason = if matches!(bad, '-' | '_' | '+') && index.starts_with(bad) {
            format!("has illegal prefix '{bad}'")
        } else {
            format!("contains illegal character '{bad}'")
        };
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': elasticsearch index '{index}' {reason}"
        )));
    }

    Ok(())
}

fn validate_loki_labels(
    pipeline_name: &str,
    label: &str,
    static_labels: Option<&HashMap<String, String>>,
    label_columns: Option<&[String]>,
) -> Result<(), ConfigError> {
    if let Some(labels) = static_labels {
        for (key, value) in labels {
            if key.trim().is_empty() || value.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': 'static_labels' keys and values must not be empty"
                )));
            }
        }
    }

    if let (Some(static_labels), Some(label_columns)) = (static_labels, label_columns)
        && let Some(conflict) = label_columns
            .iter()
            .map(String::as_str)
            .find(|column| static_labels.contains_key(*column))
    {
        return Err(ConfigError::Validation(format!(
            "pipeline '{pipeline_name}' output '{label}': loki label '{conflict}' is defined in both 'label_columns' and 'static_labels'"
        )));
    }

    Ok(())
}

pub(super) fn validate_output_config(
    pipeline_name: &str,
    label: &str,
    output: &OutputConfigV2,
) -> Result<(), ConfigError> {
    match output {
        OutputConfigV2::Otlp(config) => {
            validate_url_output_endpoint(
                pipeline_name,
                label,
                OutputType::Otlp,
                config.endpoint.as_deref(),
            )?;

            if let (Some(initial), Some(max)) =
                (config.retry_initial_backoff_ms, config.retry_max_backoff_ms)
                && initial > max
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': 'retry_initial_backoff_ms' must be <= 'retry_max_backoff_ms'"
                )));
            }
        }
        OutputConfigV2::Http(config) => {
            validate_url_output_endpoint(
                pipeline_name,
                label,
                OutputType::Http,
                config.endpoint.as_deref(),
            )?;

            if let Some(format) = &config.format
                && !matches!(format, Format::Json)
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': http output only supports format json"
                )));
            }
        }
        OutputConfigV2::Elasticsearch(config) => {
            validate_url_output_endpoint(
                pipeline_name,
                label,
                OutputType::Elasticsearch,
                config.endpoint.as_deref(),
            )?;
            validate_elasticsearch_index(pipeline_name, label, config.index.as_deref())?;

            if matches!(
                config.request_mode,
                Some(ElasticsearchRequestMode::Streaming)
            ) && matches!(config.compression, Some(CompressionFormat::Gzip))
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': elasticsearch request_mode 'streaming' does not support gzip compression yet"
                )));
            }

            if let Some(compression) = config.compression
                && !matches!(
                    compression,
                    CompressionFormat::Gzip | CompressionFormat::None
                )
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': elasticsearch compression must be 'gzip' or 'none', got '{compression}'"
                )));
            }
        }
        OutputConfigV2::Loki(config) => {
            validate_url_output_endpoint(
                pipeline_name,
                label,
                OutputType::Loki,
                config.endpoint.as_deref(),
            )?;
            validate_loki_labels(
                pipeline_name,
                label,
                config.static_labels.as_ref(),
                config.label_columns.as_deref(),
            )?;
        }
        OutputConfigV2::Stdout(config) => {
            if let Some(format) = &config.format
                && !matches!(format, Format::Json | Format::Text | Format::Console)
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': stdout output only supports format json, text, or console"
                )));
            }
        }
        OutputConfigV2::File(config) => {
            match config.path.as_deref() {
                None => {
                    return Err(ConfigError::Validation(format!(
                        "pipeline '{pipeline_name}' output '{label}': file output requires 'path'",
                    )));
                }
                Some(path) if path.trim().is_empty() => {
                    return Err(ConfigError::Validation(format!(
                        "pipeline '{pipeline_name}' output '{label}': file output 'path' must not be empty"
                    )));
                }
                Some(_) => {}
            }
            if let Some(format) = &config.format
                && !matches!(format, Format::Json | Format::Text)
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': file output only supports format json or text"
                )));
            }
        }
        OutputConfigV2::Null(_) => {}
        OutputConfigV2::Tcp(config) => {
            validate_socket_output_endpoint(
                pipeline_name,
                label,
                OutputType::Tcp,
                config.endpoint.as_deref(),
            )?;
        }
        OutputConfigV2::Udp(config) => {
            validate_socket_output_endpoint(
                pipeline_name,
                label,
                OutputType::Udp,
                config.endpoint.as_deref(),
            )?;
            if config.max_datagram_size_bytes == Some(0) {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': udp.max_datagram_size_bytes must be at least 1"
                )));
            }
        }
        OutputConfigV2::ArrowIpc(config) => {
            validate_url_output_endpoint(
                pipeline_name,
                label,
                OutputType::ArrowIpc,
                config.endpoint.as_deref(),
            )?;

            if let Some(compression) = config.compression
                && !matches!(
                    compression,
                    CompressionFormat::Zstd | CompressionFormat::None
                )
            {
                return Err(ConfigError::Validation(format!(
                    "pipeline '{pipeline_name}' output '{label}': arrow_ipc output only supports 'zstd' or 'none' compression, not '{compression}'"
                )));
            }
        }
    }

    Ok(())
}

pub(super) fn es_illegal_index_char(index: &str) -> Option<char> {
    if let Some(c) = index.chars().next()
        && matches!(c, '-' | '_' | '+')
    {
        return Some(c);
    }
    index.chars().find(|c| {
        c.is_ascii_uppercase()
            || matches!(
                c,
                '*' | '?' | '"' | '<' | '>' | '|' | ' ' | ',' | '#' | ':' | '\\' | '/'
            )
    })
}
