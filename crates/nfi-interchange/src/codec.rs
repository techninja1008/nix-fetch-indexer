//! Reading and writing bundles.
//!
//! Binary layout: `b"NFIB"`, one byte container version, one byte compression
//! id, then the (compressed) CBOR encoding of [`Bundle`].
//! JSON layout: the bundle as a single JSON object (detected by a leading `{`).

use crate::model::{Bundle, STORE_PATHS};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"NFIB";
pub const CONTAINER_VERSION: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Zstd(i32),
    Xz(u32),
    Brotli(u32),
}

impl Compression {
    pub const DEFAULT: Compression = Compression::Zstd(9);

    pub fn id(&self) -> u8 {
        match self {
            Compression::None => 0,
            Compression::Zstd(_) => 1,
            Compression::Xz(_) => 2,
            Compression::Brotli(_) => 3,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd(_) => "zstd",
            Compression::Xz(_) => "xz",
            Compression::Brotli(_) => "brotli",
        }
    }

    /// Parse `name` or `name:level`, e.g. `zstd:19`, `xz:9`, `brotli:11`, `none`.
    pub fn parse(s: &str) -> Result<Self> {
        let (name, level) = match s.split_once(':') {
            Some((n, l)) => (n, Some(l.parse::<i64>().context("compression level")?)),
            None => (s, None),
        };
        Ok(match name {
            "none" => Compression::None,
            "zstd" => Compression::Zstd(level.unwrap_or(9) as i32),
            "xz" => Compression::Xz(level.unwrap_or(9) as u32),
            "brotli" => Compression::Brotli(level.unwrap_or(11) as u32),
            _ => bail!("unknown compression {name:?} (expected none, zstd, xz, brotli)"),
        })
    }
}

impl std::str::FromStr for Compression {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Compression::parse(s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Binary(Compression),
    Json,
}

impl Format {
    pub fn parse(format: &str, compression: Option<&str>) -> Result<Self> {
        match format {
            "binary" | "bin" | "cbor" => Ok(Format::Binary(match compression {
                Some(c) => Compression::parse(c)?,
                None => Compression::DEFAULT,
            })),
            "json" => Ok(Format::Json),
            _ => bail!("unknown format {format:?} (expected binary or json)"),
        }
    }
}

/// Sizes recorded while writing a bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Bytes of the uncompressed encoding (CBOR or JSON).
    pub encoded_bytes: u64,
    /// Bytes written to the output, including the header.
    pub written_bytes: u64,
}

/// A writer that counts the bytes passing through it.
struct Counting<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn cbor_into<W: Write>(bundle: &Bundle, w: W) -> Result<u64> {
    let mut c = Counting {
        inner: std::io::BufWriter::with_capacity(1 << 20, w),
        count: 0,
    };
    ciborium::into_writer(bundle, &mut c)?;
    c.flush()?;
    Ok(c.count)
}

/// Write a bundle in the binary representation.
pub fn write_binary<W: Write>(bundle: &Bundle, w: W, compression: Compression) -> Result<WriteStats> {
    let mut w = Counting { inner: w, count: 0 };
    w.write_all(MAGIC)?;
    w.write_all(&[CONTAINER_VERSION, compression.id()])?;
    let encoded_bytes = match compression {
        Compression::None => cbor_into(bundle, &mut w)?,
        Compression::Zstd(level) => {
            let mut enc = zstd::Encoder::new(&mut w, level)?;
            enc.multithread(
                std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(1),
            )
            .ok();
            // zstd's default job size at high levels is large, leaving most
            // workers idle on bundle-sized inputs; 4 MiB jobs keep all cores
            // busy (about 40% faster at level 19, with the same ratio).
            enc.set_parameter(zstd::stream::raw::CParameter::JobSize(4 << 20))
                .ok();
            // Long-distance matching finds repeats across the whole bundle
            // (about 12% smaller at level 19).
            enc.long_distance_matching(true).ok();
            let n = cbor_into(bundle, &mut enc)?;
            enc.finish()?;
            n
        }
        Compression::Xz(level) => {
            let mut enc = xz2::write::XzEncoder::new(&mut w, level);
            let n = cbor_into(bundle, &mut enc)?;
            enc.finish()?;
            n
        }
        Compression::Brotli(level) => {
            let mut enc = brotli::CompressorWriter::new(&mut w, 1 << 16, level, 24);
            let n = cbor_into(bundle, &mut enc)?;
            enc.flush()?;
            drop(enc);
            n
        }
    };
    w.flush()?;
    Ok(WriteStats {
        encoded_bytes,
        written_bytes: w.count,
    })
}

/// Write a bundle as compact JSON.
pub fn write_json<W: Write>(bundle: &Bundle, w: W) -> Result<WriteStats> {
    let mut w = Counting {
        inner: std::io::BufWriter::with_capacity(1 << 20, w),
        count: 0,
    };
    serde_json::to_writer(&mut w, bundle)?;
    w.flush()?;
    Ok(WriteStats {
        encoded_bytes: w.count,
        written_bytes: w.count,
    })
}

pub fn write<W: Write>(bundle: &Bundle, w: W, format: Format) -> Result<WriteStats> {
    match format {
        Format::Binary(c) => write_binary(bundle, w, c),
        Format::Json => write_json(bundle, w),
    }
}

/// Write to `path` atomically (via a temporary file and rename).
pub fn write_file(bundle: &Bundle, path: &Path, format: Format) -> Result<WriteStats> {
    let tmp = path.with_extension("tmp");
    let f = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut bw = std::io::BufWriter::new(f);
    let stats = write(bundle, &mut bw, format)?;
    bw.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(stats)
}

/// The detected representation of an input stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detected {
    Binary { compression_id: u8 },
    Json,
}

/// Read a bundle in either representation (auto-detected). Version 2
/// bundles are upgraded, with their closures given `v2_closure_type`.
pub fn read<R: Read>(r: R, v2_closure_type: &str) -> Result<Bundle> {
    read_detect(r, v2_closure_type).map(|(b, _)| b)
}

pub fn read_detect<R: Read>(r: R, v2_closure_type: &str) -> Result<(Bundle, Detected)> {
    let (mut bundle, detected) = decode(r)?;
    let v2: crate::ClosureType = v2_closure_type.parse().map_err(anyhow::Error::msg)?;
    bundle.upgrade(&v2);
    Ok((bundle, detected))
}

fn decode<R: Read>(r: R) -> Result<(Bundle, Detected)> {
    let mut r = BufReader::with_capacity(1 << 16, r);
    let head = r.fill_buf()?;
    let first = head.iter().copied().find(|c| !c.is_ascii_whitespace());
    if first == Some(b'{') {
        let b: Bundle = serde_json::from_reader(r).context("parsing JSON bundle")?;
        return Ok((b, Detected::Json));
    }
    let mut header = [0u8; 6];
    r.read_exact(&mut header).context("reading bundle header")?;
    if &header[..4] != MAGIC {
        bail!("not an interchange bundle (bad magic)");
    }
    if header[4] != CONTAINER_VERSION {
        bail!("unsupported container version {}", header[4]);
    }
    let id = header[5];
    let bundle: Bundle = match id {
        0 => ciborium::from_reader(r)?,
        1 => ciborium::from_reader(BufReader::new(zstd::Decoder::with_buffer(r)?))?,
        2 => ciborium::from_reader(BufReader::new(xz2::read::XzDecoder::new(r)))?,
        3 => ciborium::from_reader(BufReader::new(brotli::Decompressor::new(r, 1 << 16)))?,
        _ => bail!("unknown compression id {id}"),
    };
    Ok((bundle, Detected::Binary { compression_id: id }))
}

pub fn read_file(path: &Path, v2_closure_type: &str) -> Result<Bundle> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read(f, v2_closure_type).with_context(|| format!("reading {}", path.display()))
}

/// Read from a path, or stdin when the path is `-`.
pub fn read_path_or_stdin(path: &str, v2_closure_type: &str) -> Result<Bundle> {
    if path == "-" {
        read(std::io::stdin().lock(), v2_closure_type)
    } else {
        read_file(Path::new(path), v2_closure_type)
    }
}

/// Result of compressing one bundle with one algorithm.
#[derive(Clone, Debug)]
pub struct BenchResult {
    pub compression: Compression,
    pub bytes: usize,
    pub encode: std::time::Duration,
    pub decode: std::time::Duration,
}

/// Encode and decode `bundle` with each compression, measuring size and time.
pub fn bench(bundle: &Bundle, compressions: &[Compression]) -> Result<Vec<BenchResult>> {
    let mut out = Vec::new();
    for &c in compressions {
        let t = std::time::Instant::now();
        let mut buf = Vec::new();
        write_binary(bundle, &mut buf, c)?;
        let encode = t.elapsed();
        let t = std::time::Instant::now();
        let back = read(&buf[..], STORE_PATHS)?;
        let decode = t.elapsed();
        if &back != bundle {
            bail!("round trip mismatch for {c:?}");
        }
        out.push(BenchResult {
            compression: c,
            bytes: buf.len(),
            encode,
            decode,
        });
    }
    Ok(out)
}
