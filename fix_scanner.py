with open('crates/logfwd-core/src/structural.rs', 'r') as f:
    content = f.read()

target = "return pos; // mismatch — fail-closed to avoid emitting truncated values"
replacement = "return end; // mismatch — fail-closed to avoid emitting truncated values"

content = content.replace(target, replacement)
with open('crates/logfwd-core/src/structural.rs', 'w') as f:
    f.write(content)

with open('crates/logfwd-core/src/json_scanner.rs', 'r') as f:
    content = f.read()

content = content.replace(target, replacement)
with open('crates/logfwd-core/src/json_scanner.rs', 'w') as f:
    f.write(content)
print("REVERTED")
