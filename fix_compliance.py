import re

with open('crates/logfwd/tests/it/compliance.rs', 'r') as f:
    content = f.read()

target = """                            arrow::datatypes::DataType::Utf8 => col.as_string::<i32>().value(row),
                            arrow::datatypes::DataType::Utf8View => col.as_string_view().value(row),
                            _ => "","""

replacement = """                            arrow::datatypes::DataType::Utf8 => col.as_string::<i32>().value(row),
                            arrow::datatypes::DataType::Utf8View => col.as_string_view().value(row),
                            arrow::datatypes::DataType::LargeUtf8 => col.as_string::<i64>().value(row),
                            _ => "","""

if target in content:
    content = content.replace(target, replacement)
    with open('crates/logfwd/tests/it/compliance.rs', 'w') as f:
        f.write(content)
    print("PATCHED")
else:
    print("NOT FOUND")
