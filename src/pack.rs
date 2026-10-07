use crate::crypt::PrivateSpec;
use crate::ecc;
use crate::ffmpeg;
use crate::frame;
use crate::inner;
use crate::manifest::{
    EccSpec, EncryptionSpec, GridSpec, Manifest, PartSpec, PayloadSpec, VideoSpec,
};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct PackOpts {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub cell: usize,
    pub cols: usize,
    pub rows: usize,
    pub fps: u32,
    pub crf: u32,
    pub group: usize,
    pub parity: usize,
    pub preset: String,
    pub dump_manifest: Option<PathBuf>,
    /// e.g. "500M", "1G", plain bytes
    pub max_part_size: Option<String>,
    /// Encrypt the payload when set (resolved, non-empty).
    pub password: Option<String>,
}

/// Parse a size like `500M`, `1G`, `1024K`, `512KB` or plain bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mul): (&str, u64) = if let Some(x) = t.strip_suffix("KB") {
        (x, 1 << 10)
    } else if let Some(x) = t.strip_suffix("MB") {
        (x, 1 << 20)
    } else if let Some(x) = t.strip_suffix("GB") {
        (x, 1 << 30)
    } else if let Some(x) = t.strip_suffix('K') {
        (x, 1 << 10)
    } else if let Some(x) = t.strip_suffix('M') {
        (x, 1 << 20)
    } else if let Some(x) = t.strip_suffix('G') {
        (x, 1 << 30)
    } else {
        (t.as_str(), 1)
    };
    let v: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size: {s}"))?;
    Ok(v.saturating_mul(mul).max(1))
}

/// `out.mp4` -> `out.part1.mp4`, `out.part2.mp4`, ...
fn part_path(base: &Path, index: u16) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let ext = match base.extension() {
        Some(e) => format!(".{}", e.to_string_lossy()),
        None => ".mp4".to_string(),
    };
    base.with_file_name(format!("{stem}.part{}{ext}", index + 1))
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn tar_dir(dir: &Path) -> Result<Vec<u8>> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());
    let mut buf = Vec::new();
    {
        let mut ar = tar::Builder::new(&mut buf);
        ar.append_dir_all(&name, dir)
            .with_context(|| format!("cannot tar {}", dir.display()))?;
        ar.finish()?;
    }
    Ok(buf)
}

/// Everything that is identical across parts of a split archive.
struct PartCtx<'a> {
    grid: GridSpec,
    nw: usize,
    nh: usize,
    fps: u32,
    crf: u32,
    preset: &'a str,
    group: usize,
    ecc: EccSpec,
    raw_len: u64,
    raw_sha256: &'a str,
    source_name: &'a str,
    source_is_dir: bool,
    compressed_total: u64,
    encryption: Option<EncryptionSpec>,
}

/// Render + encode one part; returns the final file size in bytes.
fn build_part(
    ctx: &PartCtx<'_>,
    out: &Path,
    chunk: &[u8],
    part: Option<PartSpec>,
    dump_manifest: Option<&Path>,
    t_start: std::time::Instant,
) -> Result<u64> {
    let group = ctx.group;
    let shard_bytes = ctx.ecc.shard_bytes;
    let dpg = ctx.ecc.data_per_group();
    let df = ctx.ecc.data_frames();
    let groups = chunk.len().div_ceil(dpg).max(1);
    let frames = groups * group;
    let zero = vec![0u8; shard_bytes];
    let mut shards: Vec<Vec<u8>> = vec![Vec::new(); group];

    let build_group = |g: usize, shards: &mut Vec<Vec<u8>>| -> Result<()> {
        let start = g * dpg;
        let end = (start + dpg).min(chunk.len());
        let data = &chunk[start..end];
        for (i, sh) in shards.iter_mut().enumerate() {
            if i < df {
                let d0 = i * shard_bytes;
                let d1 = (d0 + shard_bytes).min(data.len());
                if d0 < data.len() {
                    sh.clear();
                    sh.extend_from_slice(&data[d0..d1]);
                    sh.resize(shard_bytes, 0);
                } else {
                    *sh = zero.clone();
                }
            } else {
                *sh = zero.clone();
            }
        }
        ecc::encode_group(shards, df)?;
        Ok(())
    };

    // pass 1: per-frame CRCs (deterministic, no rendering needed)
    let mut frame_crc: Vec<u16> = Vec::with_capacity(frames);
    for g in 0..groups {
        build_group(g, &mut shards)?;
        for sh in shards.iter() {
            frame_crc.push(crc32fast::hash(sh) as u16);
        }
    }

    let manifest = Manifest {
        version: 1,
        grid: ctx.grid,
        ecc: ctx.ecc,
        video: VideoSpec {
            width: ctx.nw,
            height: ctx.nh,
            fps: ctx.fps,
            crf: ctx.crf,
            frames,
        },
        payload: PayloadSpec {
            raw_len: ctx.raw_len,
            raw_sha256: ctx.raw_sha256.to_string(),
            compressed_len: ctx.compressed_total,
            groups,
            source_name: ctx.source_name.to_string(),
            source_is_dir: ctx.source_is_dir,
        },
        frame_crc,
        part,
        encryption: ctx.encryption.clone(),
    };
    let blob = manifest.to_bytes();
    // keep at least two full manifest copies in the audio track, even when
    // the video itself is shorter than that requires
    let audio_samples = (frames * crate::audio::SAMPLE_RATE / ctx.fps as usize)
        .max(crate::audio::min_samples_for(blob.len(), 2));
    let label = part.map(|p| p.index).unwrap_or(0);
    let wav = std::env::temp_dir().join(format!("dmvpack-{}-{label}.wav", std::process::id()));
    crate::audio::write_wav(&wav, &blob, audio_samples)?;
    eprintln!(
        "manifest: {} bytes ({} coded), embedded in audio ({audio_samples} samples, {:.1}s) [{:.1}s]",
        blob.len(),
        blob.len().div_ceil(crate::inner::DATA_CW) * crate::inner::CW,
        audio_samples as f64 / crate::audio::SAMPLE_RATE as f64,
        t_start.elapsed().as_secs_f64()
    );
    if let Some(p) = dump_manifest {
        manifest.save(p)?;
        eprintln!("manifest dumped to {}", p.display());
    }

    let mut child = ffmpeg::spawn_encoder(
        out,
        ctx.nw,
        ctx.nh,
        ctx.fps,
        ctx.crf,
        ctx.preset,
        Some(&wav),
    )?;
    let mut stdin = child.stdin.take().expect("stdin");

    let mut t_prep = std::time::Duration::ZERO;
    let mut t_render = std::time::Duration::ZERO;
    let mut t_write = std::time::Duration::ZERO;

    for g in 0..groups {
        let ta = std::time::Instant::now();
        build_group(g, &mut shards)?;
        let tb = std::time::Instant::now();
        t_prep += tb - ta;
        let mut tc = tb;

        for sh in shards.iter() {
            let coded = inner::encode(sh);
            let img = frame::render_frame(&ctx.grid, &coded);
            let td = std::time::Instant::now();
            stdin.write_all(&img)?;
            t_render += td - tc;
            tc = std::time::Instant::now();
            t_write += tc - td;
        }
        if g % 16 == 0 {
            eprintln!(
                "  group {g}/{groups} [{:.1}s]",
                t_start.elapsed().as_secs_f64()
            );
        }
    }
    eprintln!(
        "timing: prep {:.1}s render {:.1}s write {:.1}s",
        t_prep.as_secs_f64(),
        t_render.as_secs_f64(),
        t_write.as_secs_f64()
    );
    drop(stdin);
    let status = child.wait()?;
    let _ = fs::remove_file(&wav);
    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    let size = fs::metadata(out)?.len();
    Ok(size)
}

pub fn run(o: PackOpts) -> Result<()> {
    if !o.input.exists() {
        bail!("input {} does not exist", o.input.display());
    }
    let is_dir = o.input.is_dir();
    let source_name = o
        .input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());

    if o.group < 2 || o.parity >= o.group || o.group > 64 {
        bail!(
            "--group must be > --parity >= 0, group <= 64 (got group={}, parity={})",
            o.group,
            o.parity
        );
    }
    if o.cell < 4 {
        bail!("--cell must be >= 4");
    }

    let grid = GridSpec {
        cols: o.cols,
        rows: o.rows,
        cell: o.cell,
        border: 2,
    };
    if grid.cols <= 2 * grid.border + 4 || grid.rows <= 2 * grid.border + 4 {
        bail!("grid too small");
    }
    let (nw, nh) = frame::native_size(&grid);
    if nw % 2 != 0 || nh % 2 != 0 {
        bail!("video size {nw}x{nh} must be even (yuv420p)");
    }
    let shard_bytes = inner::check_frame_layout(frame::payload_bytes(&grid))?.1;
    let limit = match &o.max_part_size {
        Some(s) => Some(parse_size(s)?),
        None => None,
    };

    // 1. raw payload: file bytes, or a tar for directories
    let t_start = std::time::Instant::now();
    let raw: Vec<u8> = if is_dir {
        tar_dir(&o.input)?
    } else {
        fs::read(&o.input).with_context(|| format!("cannot read {}", o.input.display()))?
    };
    let raw_len = raw.len() as u64;
    let raw_sha256 = sha256_hex(&raw);
    eprintln!(
        "input: {} ({} bytes{}) [{:.1}s]",
        source_name,
        raw_len,
        if is_dir { ", tarred" } else { "" },
        t_start.elapsed().as_secs_f64()
    );

    // 2. compress (pure-Rust zstd)
    let compressed =
        ruzstd::encoding::compress_to_vec(&raw[..], ruzstd::encoding::CompressionLevel::Fastest);
    drop(raw);
    let ratio = if raw_len > 0 {
        compressed.len() as f64 / raw_len as f64
    } else {
        0.0
    };
    eprintln!(
        "compressed: {} bytes (ratio {:.2}) [{:.1}s]",
        compressed.len(),
        ratio,
        t_start.elapsed().as_secs_f64()
    );

    // 2b. optional password encryption: the sealed stream replaces the plain
    // one before any ECC/frames; sensitive manifest fields move inside it
    let mut stream = compressed;
    let mut encryption = None;
    let mut pub_raw_len = raw_len;
    let mut pub_raw_sha = raw_sha256.clone();
    let mut pub_name = source_name.clone();
    let mut pub_is_dir = is_dir;
    if let Some(pw) = &o.password {
        let private = PrivateSpec {
            raw_len,
            raw_sha256: raw_sha256.clone(),
            compressed_len: stream.len() as u64,
            source_name: source_name.clone(),
            source_is_dir: is_dir,
        };
        let pj = serde_json::to_vec(&private)?;
        let (ct, spec) = crate::crypt::encrypt(&crate::crypt::wrap(&stream, &pj), pw)?;
        eprintln!(
            "encrypted: {} -> {} bytes (argon2id + chacha20-poly1305)",
            stream.len(),
            ct.len()
        );
        stream = ct;
        encryption = Some(spec);
        pub_raw_len = 0;
        pub_raw_sha = String::new();
        pub_name = String::new();
        pub_is_dir = false;
    }

    // 3. layout
    let ecc = EccSpec {
        group_frames: o.group,
        parity_frames: o.parity,
        shard_bytes,
    };
    let dpg = ecc.data_per_group();
    let total_groups = stream.len().div_ceil(dpg).max(1);
    let total_frames = total_groups * o.group;
    let duration = total_frames as f64 / o.fps as f64;
    eprintln!(
        "plan: {total_frames} frames total ({nw}x{nh}, {shard_bytes} B/frame, \
         {total_groups} groups, ~{duration:.1}s{}{})",
        match limit {
            Some(l) => format!(", split at {l} bytes per part"),
            None => String::new(),
        },
        if encryption.is_some() {
            ", encrypted"
        } else {
            ""
        }
    );

    let output = match o.output {
        Some(p) => p,
        None => {
            let mut name = o.input.file_name().unwrap_or_default().to_os_string();
            name.push(".mp4");
            PathBuf::from(name)
        }
    };

    let ctx = PartCtx {
        grid,
        nw,
        nh,
        fps: o.fps,
        crf: o.crf,
        preset: &o.preset,
        group: o.group,
        ecc,
        raw_len: pub_raw_len,
        raw_sha256: &pub_raw_sha,
        source_name: &pub_name,
        source_is_dir: pub_is_dir,
        compressed_total: stream.len() as u64,
        encryption,
    };

    // 4. encode part(s); the first cut uses a conservative size estimate and
    //    any part that overshoots the limit is rebuilt with a smaller payload
    let split = limit.is_some();
    let mut offset = 0usize;
    let mut idx: u16 = 0;
    let mut est_ratio = 3.0f64; // mp4 bytes per payload byte (initial guess)
    let mut sizes: Vec<u64> = Vec::new();

    loop {
        let remaining = stream.len() - offset;
        if remaining == 0 {
            break;
        }
        let mut pl = match limit {
            Some(lim) => (((lim as f64 * 0.95) / est_ratio) as usize)
                .max(1)
                .min(remaining),
            None => remaining,
        };
        let path = if split {
            part_path(&output, idx)
        } else {
            output.clone()
        };

        let size = loop {
            let part = split.then_some(PartSpec {
                index: idx,
                payload_offset: offset as u64,
                payload_len: pl as u64,
            });
            let dump = if idx == 0 {
                o.dump_manifest.as_deref()
            } else {
                None
            };
            let s = build_part(
                &ctx,
                &path,
                &stream[offset..offset + pl],
                part,
                dump,
                t_start,
            )?;
            match limit {
                Some(lim) if s > lim => {
                    let new_pl = ((pl as f64) * (lim as f64 / s as f64) * 0.96) as usize;
                    if new_pl == 0 || new_pl >= pl {
                        bail!(
                            "--max-part-size {lim} is too small: one part needs \
                             about {s} bytes (video group + audio manifest)"
                        );
                    }
                    eprintln!(
                        "  part {} is {s} bytes > limit {lim}, retrying with {new_pl} payload bytes",
                        idx + 1
                    );
                    pl = new_pl;
                }
                _ => break s,
            }
        };

        sizes.push(size);
        if split {
            est_ratio = size as f64 / pl as f64;
        }
        eprintln!(
            "part {}: {} ({size} bytes) [{:.1}s]",
            idx + 1,
            path.display(),
            t_start.elapsed().as_secs_f64()
        );
        offset += pl;
        if !split {
            break;
        }
        idx = idx.checked_add(1).context("too many parts")?;
    }

    let total: u64 = sizes.iter().sum();
    if split && sizes.len() == 1 {
        let src = part_path(&output, 0);
        if src != output {
            fs::rename(&src, &output)
                .with_context(|| format!("cannot rename {}", src.display()))?;
        }
        eprintln!(
            "done: {} ({total} bytes, fits in one part, manifest in audio track)",
            output.display()
        );
    } else if split {
        eprintln!(
            "done: {} parts ({} bytes total, limit {} bytes each)",
            sizes.len(),
            total,
            limit.unwrap_or(0)
        );
    } else {
        eprintln!(
            "done: {} ({total} bytes, manifest in audio track)",
            output.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_suffixes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("2K").unwrap(), 2048);
        assert_eq!(parse_size("500M").unwrap(), 500 << 20);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("512KB").unwrap(), 512 << 10);
        assert_eq!(parse_size(" 64mb ").unwrap(), 64 << 20);
        assert!(parse_size("").is_err());
        assert!(parse_size("12Q").is_err());
    }

    #[test]
    fn part_naming() {
        let p = part_path(Path::new("foxe.exe.mp4"), 0);
        assert_eq!(p, PathBuf::from("foxe.exe.part1.mp4"));
        let p = part_path(Path::new("arc.mp4"), 9);
        assert_eq!(p, PathBuf::from("arc.part10.mp4"));
        let p = part_path(Path::new("noext"), 0);
        assert_eq!(p, PathBuf::from("noext.part1.mp4"));
    }
}
