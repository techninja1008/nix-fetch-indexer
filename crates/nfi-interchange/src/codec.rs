//! Reading and writing bundles.
//!
//! Binary layout: `b"NFIB"`, one byte container version, one byte compression
//! id, then the (compressed) CBOR encoding of [`Bundle`].
//! JSON layout: the bundle as a single JSON object (detected by a leading `{`).

use crate::model::Bundle;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"NFIB";
pub const CONTAINER_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Zstd(i32),
    Xz(u32),
    Brotli(u32),
}

impl Compression {
    pub const DEFAULT: Compression = Compression::Zstd(19);

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
            "zstd" => Compression::Zstd(level.unwrap_or(19) as i32),
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

/// Write a bundle in the binary representation.
pub fn write_binary<W: Write>(bundle: &Bundle, mut w: W, compression: Compression) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&[CONTAINER_VERSION, compression.id()])?;
    match compression {
        Compression::None => {
            ciborium::into_writer(bundle, &mut w)?;
        }
        Compression::Zstd(level) => {
            let mut enc = zstd::Encoder::new(&mut w, level)?;
            enc.multithread(
                std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(1),
            )
            .ok();
            enc.long_distance_matching(true).ok();
            ciborium::into_writer(bundle, std::io::BufWriter::new(&mut enc))?;
            enc.finish()?;
        }
        Compression::Xz(level) => {
            let mut enc = xz2::write::XzEncoder::new(&mut w, level);
            ciborium::into_writer(bundle, std::io::BufWriter::new(&mut enc))?;
            enc.finish()?;
        }
        Compression::Brotli(level) => {
            let mut enc = brotli::CompressorWriter::new(&mut w, 1 << 16, level, 24);
            ciborium::into_writer(bundle, std::io::BufWriter::new(&mut enc))?;
            enc.flush()?;
        }
    }
    w.flush()?;
    Ok(())
}

/// Write a bundle as compact JSON.
pub fn write_json<W: Write>(bundle: &Bundle, w: W) -> Result<()> {
    let mut w = std::io::BufWriter::new(w);
    serde_json::to_writer(&mut w, bundle)?;
    w.flush()?;
    Ok(())
}

pub fn write<W: Write>(bundle: &Bundle, w: W, format: Format) -> Result<()> {
    match format {
        Format::Binary(c) => write_binary(bundle, w, c),
        Format::Json => write_json(bundle, w),
    }
}

pub fn write_file(bundle: &Bundle, path: &Path, format: Format) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let f = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    write(bundle, std::io::BufWriter::new(f), format)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The detected representation of an input stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detected {
    Binary { compression_id: u8 },
    Json,
}

/// Read a bundle in either representation (auto-detected).
pub fn read<R: Read>(r: R) -> Result<Bundle> {
    read_detect(r).map(|(b, _)| b)
}

pub fn read_detect<R: Read>(r: R) -> Result<(Bundle, Detected)> {
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

pub fn read_file(path: &Path) -> Result<Bundle> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read(f).with_context(|| format!("reading {}", path.display()))
}

/// Read from a path, or stdin when the path is `-`.
pub fn read_path_or_stdin(path: &str) -> Result<Bundle> {
    if path == "-" {
        read(std::io::stdin().lock())
    } else {
        read_file(Path::new(path))
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
        let back = read(&buf[..])?;
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
