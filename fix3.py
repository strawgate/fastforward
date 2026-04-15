with open("crates/logfwd-arrow/src/star_schema.rs", "r") as f:
    content = f.read()

content = content.replace('''fn str_from_array(arr: &dyn Array, row: usize) -> String {
    if arr.is_null(row) {
        return String::new();
    }
    match arr.data_type() {
        DataType::Utf8 => arr
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        DataType::Utf8View => arr
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}''', '''fn str_from_array(arr: &dyn Array, row: usize) -> String {
    if arr.is_null(row) {
        return String::new();
    }
    match arr.data_type() {
        DataType::Utf8 => arr
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        DataType::Utf8View => arr
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}''')

content = content.replace('let val = str_value_at(arr.as_ref(), row);\n                    str_vals.push(if val.is_empty() { None } else { Some(val) });', 'let val = str_value_at(arr.as_ref(), row);\n                    str_vals.push(Some(val));')
content = content.replace('str_vals.push(if val.is_empty() { None } else { Some(val) });', 'str_vals.push(Some(val));')

content = content.replace('let s = str_value_at(arr.as_ref(), row);\n                    if s.is_empty() { None } else { Some(s) }', 'let s = str_value_at(arr.as_ref(), row);\n                    Some(s)')


content = content.replace('''        // Write the typed value into the column.
        match type_tag {
            ATTR_TYPE_INT => {''', '''        // Write the typed value into the column.
        match type_tag {
            ATTR_TYPE_INT => {''')

content = content.replace('''            ATTR_TYPE_STR => {}
            _ => unreachable!("type tags are validated before dispatch"),
        }''', '''            ATTR_TYPE_STR => {}
            _ => return Err(ArrowError::SchemaError(format!("unsupported column type: {}", type_tag))),
        }''')

with open("crates/logfwd-arrow/src/star_schema.rs", "w") as f:
    f.write(content)
