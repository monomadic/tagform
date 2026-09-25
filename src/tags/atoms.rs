//! Container structure: where `moov` sits relative to `mdat` (DESIGN §9.2).
//!
//! A Rust port of mp4doctor's `atom_state()`. tagform does not repair
//! containers -- that stays mp4doctor's job -- but it must be able to *verify*
//! that a remux it asked to be faststart actually came out faststart, and to
//! know whether a file already is before deciding it needs a remux at all.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `moov` precedes `mdat`: playable before the whole file arrives.
    FastStart,
    /// `mdat` precedes `moov`: the classic "moov at end".
    MoovAtEnd,
    /// Any `moof` at all. Repairable, but not by this tool.
    Fragmented,
    /// Truncated, not an ISO container, or an atom chain that does not add up.
    Inconclusive,
}

impl Layout {
    pub fn is_faststart(self) -> bool {
        self == Layout::FastStart
    }
}

/// Walk the top-level atom chain. Any `moof` means fragmented; a real `mdat`
/// settles the verdict by whether a `moov` came first.
pub fn layout(path: &Path) -> Layout {
    match scan(path) {
        Ok(l) => l,
        Err(_) => Layout::Inconclusive,
    }
}

fn scan(path: &Path) -> std::io::Result<Layout> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut pos: u64 = 0;
    let mut seen_moov = false;

    // Bounded so a hostile or corrupt file cannot spin here.
    for _ in 0..100_000 {
        if pos + 8 > size {
            return Ok(Layout::Inconclusive);
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 8];
        if f.read_exact(&mut hdr).is_err() {
            return Ok(Layout::Inconclusive);
        }
        let mut atom_size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let kind = &hdr[4..8];
        let mut header_len: u64 = 8;

        if atom_size == 1 {
            // 64-bit extended size.
            let mut ext = [0u8; 8];
            if f.read_exact(&mut ext).is_err() {
                return Ok(Layout::Inconclusive);
            }
            atom_size = u64::from_be_bytes(ext);
            header_len = 16;
        } else if atom_size == 0 {
            // Runs to end of file.
            atom_size = size - pos;
        }
        if atom_size < header_len {
            return Ok(Layout::Inconclusive);
        }

        match kind {
            b"moof" => return Ok(Layout::Fragmented),
            b"mdat" => {
                return Ok(if seen_moov { Layout::FastStart } else { Layout::MoovAtEnd })
            }
            b"moov" => seen_moov = true,
            _ => {}
        }
        pos = match pos.checked_add(atom_size) {
            Some(p) => p,
            None => return Ok(Layout::Inconclusive),
        };
    }
    Ok(Layout::Inconclusive)
}

/// The `mvhd` creation and modification times, in the container's own unit:
/// seconds since 1904-01-01 UTC. Zero is "never set", which is what a plain
/// ffmpeg mux leaves and what ffprobe then omits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Times {
    pub creation: u64,
    pub modification: u64,
}

/// A file's `mvhd` times, or `None` when there is no readable `moov`.
pub fn times(path: &Path) -> Option<Times> {
    let (_, moov) = read_moov(path).ok()??;
    let mut found = None;
    walk(&moov, 0, moov.len(), &mut |kind, off, size| {
        if kind == b"mvhd" && found.is_none() {
            found = read_times(&moov, off, size);
        }
    });
    found
}

/// Write `t` into `mvhd` and into every `tkhd` and `mdhd`, in place, and
/// read it back. This is the one thing the remux edits after ffmpeg has run:
/// the plan clears `creation_time` so ffmpeg does not promote it to a tag
/// (plan::junk_clears), and that zeroes every one of these -- so the capture
/// time is put back here, exactly as ffmpeg itself would have set it.
pub fn restore_times(path: &Path, t: Times) -> anyhow::Result<()> {
    use std::io::Write;
    let (start, moov) = read_moov(path)?
        .ok_or_else(|| anyhow::anyhow!("{}: no moov to restore the capture time into", path.display()))?;
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    walk(&moov, 0, moov.len(), &mut |kind, off, size| {
        if matches!(kind, b"mvhd" | b"tkhd" | b"mdhd") {
            if let Some((at, bytes)) = encode_times(&moov, off, size, t) {
                patches.push((start + at as u64, bytes));
            }
        }
    });
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    for (at, bytes) in patches {
        f.seek(SeekFrom::Start(at))?;
        f.write_all(&bytes)?;
    }
    f.sync_all()?;
    let got = times(path);
    if got != Some(t) {
        anyhow::bail!("restoring the capture time: wrote {t:?}, read back {got:?}");
    }
    Ok(())
}

/// The whole `moov` and where it starts, read the way `native::survey` reads
/// it: only that box, never the media.
fn read_moov(path: &Path) -> std::io::Result<Option<(u64, Vec<u8>)>> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut pos: u64 = 0;
    for _ in 0..100_000 {
        if pos + 8 > size {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 16];
        let n = f.read(&mut hdr)?;
        if n < 8 {
            return Ok(None);
        }
        let (atom_size, header_len) = match u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) {
            1 if n >= 16 => (u64::from_be_bytes(hdr[8..16].try_into().unwrap()), 16u64),
            0 => (size - pos, 8),
            v => (v as u64, 8),
        };
        if atom_size < header_len {
            return Ok(None);
        }
        if &hdr[4..8] == b"moov" {
            let mut buf = vec![0u8; atom_size.min(size - pos) as usize];
            f.seek(SeekFrom::Start(pos))?;
            f.read_exact(&mut buf)?;
            return Ok(Some((pos, buf)));
        }
        pos = match pos.checked_add(atom_size) {
            Some(p) => p,
            None => return Ok(None),
        };
    }
    Ok(None)
}

/// Visit every box in `buf[off..end]`, descending into the ones that hold
/// the headers with times in them. `f` gets the kind, the box's offset in
/// `buf`, and its size.
fn walk(buf: &[u8], mut off: usize, end: usize, f: &mut dyn FnMut(&[u8], usize, usize)) {
    while off + 8 <= end {
        let (size, hlen) = match u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) {
            1 if off + 16 <= end => {
                (u64::from_be_bytes(buf[off + 8..off + 16].try_into().unwrap()) as usize, 16)
            }
            0 => (end - off, 8),
            n => (n as usize, 8),
        };
        if size < hlen || off + size > end {
            return;
        }
        let kind = &buf[off + 4..off + 8];
        f(kind, off, size);
        if matches!(kind, b"moov" | b"trak" | b"mdia") {
            walk(buf, off + hlen, off + size, f);
        }
        off += size;
    }
}

/// `mvhd`, `tkhd` and `mdhd` are FullBoxes: version and flags first, then
/// the two times, 32-bit in version 0 and 64-bit in version 1.
fn read_times(buf: &[u8], off: usize, size: usize) -> Option<Times> {
    let body = off + 8;
    let v = *buf.get(body)?;
    let at = body + 4;
    match v {
        0 if at + 8 <= off + size => Some(Times {
            creation: u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as u64,
            modification: u32::from_be_bytes(buf[at + 4..at + 8].try_into().unwrap()) as u64,
        }),
        1 if at + 16 <= off + size => Some(Times {
            creation: u64::from_be_bytes(buf[at..at + 8].try_into().unwrap()),
            modification: u64::from_be_bytes(buf[at + 8..at + 16].try_into().unwrap()),
        }),
        _ => None,
    }
}

/// The bytes that put `t` into the box at `off`, and where in `buf` they go.
/// A time that does not fit a version-0 box is left alone rather than
/// truncated; that is a file from after 2040.
fn encode_times(buf: &[u8], off: usize, size: usize, t: Times) -> Option<(usize, Vec<u8>)> {
    let body = off + 8;
    let v = *buf.get(body)?;
    let at = body + 4;
    match v {
        0 if at + 8 <= off + size => {
            let c = u32::try_from(t.creation).ok()?;
            let m = u32::try_from(t.modification).ok()?;
            let mut b = c.to_be_bytes().to_vec();
            b.extend_from_slice(&m.to_be_bytes());
            Some((at, b))
        }
        1 if at + 16 <= off + size => {
            let mut b = t.creation.to_be_bytes().to_vec();
            b.extend_from_slice(&t.modification.to_be_bytes());
            Some((at, b))
        }
        _ => None,
    }
}

/// Free bytes on the volume holding `dir`.
///
/// The remux is atomic-by-copy: it needs room for a second full-size file. A
/// 10 GB file on a volume with 4 GB free fails with nothing wrong with the
/// container at all, and reporting that as "could not write tags" sends you
/// looking for corruption that is not there -- so it is checked up front and
/// reported as itself. Same reasoning as mp4doctor's free_bytes_for.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-k")
        .arg(dir)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().last()?;
    let avail_kb: u64 = last.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_inconclusive_not_a_panic() {
        assert_eq!(layout(Path::new("/nonexistent/nope.mp4")), Layout::Inconclusive);
    }

    #[test]
    fn garbage_is_inconclusive() {
        let p = std::env::temp_dir().join("tagform-atoms-garbage.bin");
        std::fs::write(&p, b"this is not an mp4 at all, not even close").unwrap();
        assert_eq!(layout(&p), Layout::Inconclusive);
        std::fs::remove_file(&p).ok();
    }

    /// A hand-built chain: ftyp, then moov, then mdat -> faststart.
    #[test]
    fn moov_before_mdat_is_faststart() {
        let p = std::env::temp_dir().join("tagform-atoms-fast.bin");
        std::fs::write(&p, chain(&[(b"ftyp", 8), (b"moov", 16), (b"mdat", 32)])).unwrap();
        assert_eq!(layout(&p), Layout::FastStart);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn mdat_before_moov_is_moov_at_end() {
        let p = std::env::temp_dir().join("tagform-atoms-slow.bin");
        std::fs::write(&p, chain(&[(b"ftyp", 8), (b"mdat", 32), (b"moov", 16)])).unwrap();
        assert_eq!(layout(&p), Layout::MoovAtEnd);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn any_moof_is_fragmented() {
        let p = std::env::temp_dir().join("tagform-atoms-frag.bin");
        std::fs::write(&p, chain(&[(b"ftyp", 8), (b"moov", 16), (b"moof", 16), (b"mdat", 32)]))
            .unwrap();
        assert_eq!(layout(&p), Layout::Fragmented);
        std::fs::remove_file(&p).ok();
    }

    /// An atom claiming to be smaller than its own header must not loop.
    #[test]
    fn a_nonsense_size_terminates() {
        let p = std::env::temp_dir().join("tagform-atoms-zero.bin");
        let mut v = Vec::new();
        v.extend_from_slice(&3u32.to_be_bytes());
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(&[0; 16]);
        std::fs::write(&p, v).unwrap();
        assert_eq!(layout(&p), Layout::Inconclusive);
        std::fs::remove_file(&p).ok();
    }

    /// A hand-built moov with a version-0 mvhd and one trak: the times go in
    /// and come back, in every header, without touching the bytes around them.
    #[test]
    fn times_round_trip_through_every_header() {
        fn full(kind: &[u8; 4], version: u8, body_len: usize) -> Vec<u8> {
            let mut v = ((8 + 4 + body_len) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.push(version);
            v.extend_from_slice(&[0, 0, 0]);
            v.extend(std::iter::repeat_n(0xAA, body_len));
            v
        }
        fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        let mdia = boxed(b"mdia", &full(b"mdhd", 1, 28));
        let mut trak = full(b"tkhd", 0, 80);
        trak.extend_from_slice(&mdia);
        let mut moov = full(b"mvhd", 0, 96);
        moov.extend_from_slice(&boxed(b"trak", &trak));
        let mut file = boxed(b"ftyp", b"isom");
        file.extend_from_slice(&boxed(b"moov", &moov));
        file.extend_from_slice(&boxed(b"mdat", &[0; 8]));

        let p = std::env::temp_dir().join(format!("tagform-atoms-times-{}.bin", std::process::id()));
        std::fs::write(&p, &file).unwrap();
        assert_eq!(times(&p), Some(Times { creation: 0xAAAAAAAA, modification: 0xAAAAAAAA }));

        let t = Times { creation: 3_790_000_739, modification: 3_790_000_740 };
        restore_times(&p, t).unwrap();
        assert_eq!(times(&p), Some(t));

        // Every header took it, and only the time bytes changed.
        let after = std::fs::read(&p).unwrap();
        assert_eq!(after.len(), file.len());
        let (mut seen, mut changed) = (0, 0);
        walk(&after, 0, after.len(), &mut |kind, off, size| {
            if matches!(kind, b"mvhd" | b"tkhd" | b"mdhd") {
                seen += 1;
                assert_eq!(read_times(&after, off, size), Some(t));
            }
        });
        for (a, b) in file.iter().zip(after.iter()) {
            if a != b {
                changed += 1;
            }
        }
        assert_eq!(seen, 3);
        assert_eq!(changed, 8 + 8 + 16);
        std::fs::remove_file(&p).ok();
    }

    fn chain(atoms: &[(&[u8; 4], u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (kind, size) in atoms {
            v.extend_from_slice(&size.to_be_bytes());
            v.extend_from_slice(*kind);
            v.extend(std::iter::repeat_n(0, *size as usize - 8));
        }
        v
    }
}
