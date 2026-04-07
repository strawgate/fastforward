with open("crates/logfwd-output/src/lib.rs", "r") as f:
    content = f.read()

content = content.replace("3.140_f32", "1.234_f32")
content = content.replace("3.140_f64", "1.234_f64")
content = content.replace("vec![3.140]", "vec![1.234]")
content = content.replace("unwrap() - 3.140).abs()", "unwrap() - 1.234).abs()")

with open("crates/logfwd-output/src/lib.rs", "w") as f:
    f.write(content)

with open("crates/logfwd-arrow/src/streaming_builder.rs", "r") as f:
    content = f.read()

content = content.replace("3.14", "1.234")

with open("crates/logfwd-arrow/src/streaming_builder.rs", "w") as f:
    f.write(content)
