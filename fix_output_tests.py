with open("crates/logfwd-output/src/lib.rs", "r") as f:
    content = f.read()

content = content.replace("3.14_f32", "3.141_592_f32")
content = content.replace("3.14_f64", "3.141_592_653_5_f64")
content = content.replace("vec![3.14]", "vec![3.141_592_653_5]")
content = content.replace("unwrap() - 3.14).abs()", "unwrap() - 3.141_592_653_5).abs()")

with open("crates/logfwd-output/src/lib.rs", "w") as f:
    f.write(content)
