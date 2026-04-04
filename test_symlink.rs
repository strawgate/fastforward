use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub fingerprint: u64,
}

fn compute_fingerprint(file: &mut File, max_bytes: usize) -> io::Result<u64> {
    let pos = file.stream_position()?;
    file.seek(SeekFrom::Start(0))?;

    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf)?;

    file.seek(SeekFrom::Start(pos))?;

    if n == 0 {
        return Ok(0);
    }
    Ok(1) // dummy
}

fn identify_file_old(path: &Path, fingerprint_bytes: usize) -> io::Result<FileIdentity> {
    let meta = fs::metadata(path)?;
    let mut file = File::open(path)?;
    let fingerprint = compute_fingerprint(&mut file, fingerprint_bytes)?;
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        fingerprint,
    })
}

fn identify_file_new(path: &Path, fingerprint_bytes: usize) -> io::Result<FileIdentity> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let fingerprint = compute_fingerprint(&mut file, fingerprint_bytes)?;
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        fingerprint,
    })
}

fn main() {}
