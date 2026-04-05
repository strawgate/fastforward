import re

files = [
    'crates/logfwd-io/src/arrow_ipc_receiver.rs',
    'crates/logfwd-io/src/otap_receiver.rs',
    'crates/logfwd-io/src/otlp_receiver.rs'
]

for filepath in files:
    with open(filepath, 'r') as f:
        content = f.read()

    content = content.replace('_handle: Option<std::thread::JoinHandle<()>>', 'handle: Option<std::thread::JoinHandle<()>>')
    content = content.replace('_handle: Some(handle)', 'handle: Some(handle)')
    content = content.replace('self._handle.take()', 'self.handle.take()')

    with open(filepath, 'w') as f:
        f.write(content)
