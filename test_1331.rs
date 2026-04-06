use logfwd_config::{Config, InputType};
fn main() {
    let yaml = r#"
input:
  type: arrow_ipc
output:
  type: stdout
"#;
    println!("{:?}", Config::load_str(yaml));
}
