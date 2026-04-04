use std::fs::{self, File};
use std::io::Write;
use std::time::Duration;
use logfwd_io::tail::{FileTailer, TailConfig, TailEvent};

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("trunc_drain.log");

    {
        let mut f = File::create(&log_path).unwrap();
        writeln!(f, "initial").unwrap();
    }

    let config = TailConfig {
        start_from_end: false,
        poll_interval_ms: 10,
        ..Default::default()
    };
    let mut tailer = FileTailer::new(std::slice::from_ref(&log_path), config).unwrap();

    std::thread::sleep(Duration::from_millis(50));
    tailer.poll().unwrap();

    // Now, simulate a copytruncate ON the file while it's still open,
    // and then delete it.
    {
        let mut f = File::create(&log_path).unwrap(); // truncates to 0
        writeln!(f, "after truncate").unwrap();
    }
    fs::remove_file(&log_path).unwrap(); // unlink

    std::thread::sleep(Duration::from_millis(50));
    let events = tailer.poll().unwrap();

    let has_truncated = events.iter().any(|e| matches!(e, TailEvent::Truncated { .. }));
    println!("has_truncated: {}", has_truncated);
}
