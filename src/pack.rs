use crate::ecc;
use crate::ffmpeg;
use crate::frame;
use crate::inner;
use crate::manifest::{EccSpec, GridSpec, Manifest, PayloadSpec, VideoSpec};

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

    // 3. layout
    let ecc = EccSpec {
        group_frames: o.group,
        parity_frames: o.parity,
        shard_bytes,
    };
    let data_per_group = ecc.data_per_group();
    let groups = compressed.len().div_ceil(data_per_group).max(1);
    let frames = groups * o.group;
    let duration = frames as f64 / o.fps as f64;
    eprintln!(
        "plan: {frames} frames ({}x{}, {shard_bytes} B/frame, {groups} groups, ~{duration:.1}s)",
        nw, nh
    );

    let output = match o.output {
        Some(p) => p,
        None => {
            let mut name = o.input.file_name().unwrap_or_default().to_os_string();
            name.push(".mp4");
            PathBuf::from(name)
        }
    };

    let zero = vec![0u8; shard_bytes];
    let mut shards: Vec<Vec<u8>> = vec![Vec::new(); o.group];
    let df = ecc.data_frames();

    let build_group = |g: usize, shards: &mut Vec<Vec<u8>>| -> Result<()> {
        let start = g * data_per_group;
        let end = (start + data_per_group).min(compressed.len());
        let data = &compressed[start..end];
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
    eprintln!(
        "  pass 1: frame crcs [{:.1}s]",
        t_start.elapsed().as_secs_f64()
    );

    // 4. manifest embedded in the audio track
    let manifest = Manifest {
        version: 1,
        grid,
        ecc,
        video: VideoSpec {
            width: nw,
            height: nh,
            fps: o.fps,
            crf: o.crf,
            frames,
        },
        payload: PayloadSpec {
            raw_len,
            raw_sha256,
            compressed_len: compressed.len() as u64,
            groups,
            source_name,
            source_is_dir: is_dir,
        },
        frame_crc,
    };
    let blob = manifest.to_bytes();
    // keep at least two full manifest copies in the audio track, even when
    // the video itself is shorter than that requires
    let audio_samples = (frames * crate::audio::SAMPLE_RATE / o.fps as usize)
        .max(crate::audio::min_samples_for(blob.len(), 2));
    let wav = std::env::temp_dir().join(format!("vidarc-{}.wav", std::process::id()));
    crate::audio::write_wav(&wav, &blob, audio_samples)?;
    eprintln!(
        "manifest: {} bytes ({} coded), embedded in audio ({audio_samples} samples, {:.1}s) [{:.1}s]",
        blob.len(),
        blob.len().div_ceil(crate::inner::DATA_CW) * crate::inner::CW,
        audio_samples as f64 / crate::audio::SAMPLE_RATE as f64,
        t_start.elapsed().as_secs_f64()
    );
    if let Some(p) = &o.dump_manifest {
        manifest.save(p)?;
        eprintln!("manifest dumped to {}", p.display());
    }

    // 5. render + encode (ffmpeg reads video frames from the pipe and the
    //    manifest-carrying wav as a second input)
    let mut child = ffmpeg::spawn_encoder(&output, nw, nh, o.fps, o.crf, &o.preset, Some(&wav))?;
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
            let img = frame::render_frame(&grid, &coded);
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
    eprintln!(
        "  frames rendered [{:.1}s]",
        t_start.elapsed().as_secs_f64()
    );
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    let _ = fs::remove_file(&wav);
    eprintln!("  ffmpeg done [{:.1}s]", t_start.elapsed().as_secs_f64());

    let vsize = fs::metadata(&output)?.len();
    eprintln!(
        "done: {} ({} bytes, manifest in audio track)",
        output.display(),
        vsize
    );
    Ok(())
}
