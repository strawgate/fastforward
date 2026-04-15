import re

with open('crates/logfwd-output/src/loki.rs', 'r') as f:
    c = f.read()

# request_timeout_ms is not dead code, we removed the `#[allow(dead_code)]` from it already and it's used inside new(). Wait, I might have messed up new() args earlier when reverting. Let's check `request_timeout_ms` usages.
