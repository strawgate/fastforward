use crate::types::OutputType;

pub(crate) fn parse_output_type_name(value: &str) -> Option<OutputType> {
    match value {
        "otlp" => Some(OutputType::Otlp),
        "http" => Some(OutputType::Http),
        "elasticsearch" => Some(OutputType::Elasticsearch),
        "loki" => Some(OutputType::Loki),
        "stdout" => Some(OutputType::Stdout),
        "file" => Some(OutputType::File),
        "parquet" => Some(OutputType::Parquet),
        "null" => Some(OutputType::Null),
        "tcp" => Some(OutputType::Tcp),
        "udp" => Some(OutputType::Udp),
        "arrow_ipc" => Some(OutputType::ArrowIpc),
        _ => None,
    }
}

pub(crate) fn supported_output_type_names_for_errors() -> &'static [&'static str] {
    &[
        "otlp",
        "http",
        "elasticsearch",
        "loki",
        "stdout",
        "file",
        "parquet",
        "null",
        "tcp",
        "udp",
        "arrow_ipc",
    ]
}
