import re

file_path = "crates/logfwd-runtime/src/pipeline/input_build.rs"
with open(file_path, "r") as f:
    content = f.read()

content = content.replace("""                start_from_end: f
                    .start_position
                    .as_ref()
                    .is_none_or(|pos| matches!(pos, logfwd_config::FileStartPosition::End)),""", """                start_from_end: f
                    .start_position
                    .as_ref()
                    .map_or(false, |pos| matches!(pos, logfwd_config::FileStartPosition::End)),""")

content = content.replace("""                        let mut tail_config = TailConfig {""", """            let mut tail_config = TailConfig {""")

with open(file_path, "w") as f:
    f.write(content)
