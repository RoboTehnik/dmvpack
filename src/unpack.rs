use crate::ecc;
use crate::ffmpeg;
use crate::frame::{find_alignment, payload_bytes, Sampler};
use crate::inner;
use crate::manifest::Manifest;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// Sample the payload grid of one frame and run the inner RS decode.
/// Returns the data portion of the codewords, or None if unrecoverable.
fn sample_data(sampler: &Sampler, img: &[u8], ncw: usize) -> Option<Vec<u8>> {
    let payload = sampler.payload(img);
    inner::decode(&payload[..ncw * inner::CW])
}

/// Find timeline offset t: original frame i is expected at video index j = i - t.
fn find_time_offset(video_crc: &[Option<u16>], orig: &[u16]) -> (i64, f64) {
    let v = video_crc.len() as i64;
    let e = orig.len() as i64;
    if v == 0 || e == 0 {
        return (0, 0.0);
    }
    let tmin = (e - v).min(0);
    let tmax = (v - 1).max(tmin);

    let sample_count = 64usize.min(e as usize);
    let step = ((e as usize) / sample_count).max(1);

    let mut best_t = 0i64;
    let mut best_m = 0usize;
    let mut total = 0usize;

    let mut t = tmin;
    while t <= tmax {
        let mut m = 0usize;
        let mut n = 0usize;
        let mut i = 0usize;
        while i < e as usize {
            n += 1;
            let j = i as i64 - t;
            if j >= 0 && j < v && video_crc[j as usize] == Some(orig[i]) {
                m += 1;
            }
            i += step;
        }
        total = n;
        if m > best_m {
            best_m = m;
            best_t = t;
        }
        t += 1;
    }
    let rate = if total > 0 {
        best_m as f64 / total as f64
    } else {
        0.0
    };
    (best_t, rate)
}

pub struct UnpackOpts {
    pub video: PathBuf,
    pub manifest: Option<PathBuf>,
    pub outdir: PathBuf,
}

pub fn run(o: UnpackOpts) -> Result<()> {
    ffmpeg::check_video(&o.video)?;
    let man = match &o.manifest {
        Some(p) => {
            eprintln!("manifest override: {}", p.display());
            Manifest::load(p)?
        }
        None => {
            let t0 = std::time::Instant::now();
            let pcm = ffmpeg::extract_audio_pcm(&o.video)?;
            eprintln!(
                "  audio: {} samples ({:.1}s) [{:.1}s]",
                pcm.len(),
                pcm.len() as f64 / crate::audio::SAMPLE_RATE as f64,
                t0.elapsed().as_secs_f64()
            );
            let blob = crate::audio::demodulate(&pcm)?;
            eprintln!(
                "  manifest recovered from audio ({} bytes) [{:.1}s]",
                blob.to_bytes().len(),
                t0.elapsed().as_secs_f64()
            );
            blob
        }
    };
    let (gw, gh) = ffmpeg::probe_size(&o.video)?;
    let g = man.grid;
    let (ncw, data_len) = inner::check_frame_layout(payload_bytes(&g))?;
    if man.ecc.shard_bytes != data_len {
        bail!(
            "manifest shard_bytes {} does not match inner layout {} for grid {g:?}",
            man.ecc.shard_bytes,
            data_len
        );
    }
    let group = man.ecc.group_frames;
    let df = man.ecc.data_frames();
    let e = man.frame_crc.len();
    eprintln!(
        "video {} ({}x{}), expecting {} frames, {} bytes/frame",
        o.video.display(),
        gw,
        gh,
        e,
        man.ecc.shard_bytes
    );

    // ---- pass A: per-frame CRCs over the whole video ----
    let t_start = std::time::Instant::now();
    let mut alignment: Option<(i32, i32, f64)> = None;
    let mut video_crc: Vec<Option<u16>> = Vec::new();
    let mut inner_ok = 0usize;
    let mut tried = 0usize;
    {
        let mut rd = ffmpeg::FrameReader::open(&o.video, gw, gh)?;
        while let Some(img) = rd.next_frame()? {
            if alignment.is_none() && tried < 5 {
                let (ox, oy, score) = find_alignment(&g, &img, gw, gh);
                tried += 1;
                eprintln!("  alignment try #{tried}: offset ({ox},{oy}), border match {score:.2}");
                if score >= 0.90 || tried == 5 {
                    alignment = Some((ox, oy, score));
                }
            }
            if let Some((ox, oy, _)) = alignment {
                let sampler = Sampler::new(g, gw, gh, ox, oy);
                match sample_data(&sampler, &img, ncw) {
                    Some(d) => {
                        inner_ok += 1;
                        video_crc.push(Some(crc32fast::hash(&d) as u16));
                    }
                    None => video_crc.push(None),
                }
            } else {
                video_crc.push(None);
            }
        }
    }
    let (ox, oy, score) = alignment.context("video has no frames")?;
    if score < 0.90 {
        eprintln!("warning: low border match ({score:.2}); decoding anyway");
    }
    let v = video_crc.len();
    if v == 0 {
        bail!("no frames decoded from video");
    }
    eprintln!(
        "  pass A done: {v} frames, {inner_ok} inner-decoded [{:.1}s]",
        t_start.elapsed().as_secs_f64()
    );

    // ---- timeline alignment ----
    let (t, rate) = find_time_offset(&video_crc, &man.frame_crc);
    eprintln!(
        "timeline: offset {t} frames, match rate {rate:.2} [{:.1}s]",
        t_start.elapsed().as_secs_f64()
    );
    if rate < 0.50 {
        bail!(
            "cannot align video to manifest (match rate {rate:.2}); \
             the video was probably re-cut or heavily re-encoded"
        );
    }

    // ---- pass B: extract payload shards ----
    let groups = man.payload.groups;
    let mut shards: Vec<Vec<(Vec<u8>, bool)>> =
        vec![vec![(vec![0u8; man.ecc.shard_bytes], false); group]; groups];
    let mut matched = 0usize;
    let mut mismatched = 0usize;
    let mut outside = 0usize;
    {
        let sampler = Sampler::new(g, gw, gh, ox, oy);
        let mut rd = ffmpeg::FrameReader::open(&o.video, gw, gh)?;
        let mut j: i64 = 0;
        while let Some(img) = rd.next_frame()? {
            let i = j + t;
            if i >= 0 && (i as usize) < e {
                let i = i as usize;
                let gi = i / group;
                let fi = i % group;
                match sample_data(&sampler, &img, ncw) {
                    Some(d) if crc32fast::hash(&d) as u16 == man.frame_crc[i] => {
                        shards[gi][fi] = (d, true);
                        matched += 1;
                    }
                    _ => mismatched += 1,
                }
            } else {
                outside += 1;
            }
            j += 1;
        }
    }
    eprintln!(
        "frames: {matched} verified, {mismatched} damaged, {outside} outside timeline [{:.1}s]",
        t_start.elapsed().as_secs_f64()
    );

    // ---- RS repair + concatenation ----
    let mut data = Vec::with_capacity(man.payload.compressed_len as usize);
    for (gi, group_shards) in shards.iter_mut().enumerate() {
        let missing = group_shards.iter().filter(|s| !s.1).count();
        if missing > 0 {
            ecc::repair_group(group_shards, df)
                .with_context(|| format!("group {gi} is unrecoverable"))?;
        }
        for s in &group_shards[..df] {
            data.extend_from_slice(&s.0);
        }
    }
    data.truncate(man.payload.compressed_len as usize);

    // ---- decompress ----
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&data[..])
        .map_err(|e| anyhow::anyhow!("zstd frame init: {e}"))?;
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| anyhow::anyhow!("zstd decompress: {e}"))?;

    // ---- integrity ----
    let got_sha = sha256_hex(&raw);
    if got_sha != man.payload.raw_sha256 {
        bail!(
            "payload checksum mismatch: got {got_sha}, expected {}",
            man.payload.raw_sha256
        );
    }
    if raw.len() as u64 != man.payload.raw_len {
        bail!(
            "payload length mismatch: got {}, expected {}",
            raw.len(),
            man.payload.raw_len
        );
    }
    eprintln!("payload verified: {} bytes, sha256 ok", raw.len());

    // ---- extract ----
    std::fs::create_dir_all(&o.outdir)
        .with_context(|| format!("cannot create {}", o.outdir.display()))?;
    let dest = o.outdir.join(&man.payload.source_name);
    if man.payload.source_is_dir {
        let mut ar = tar::Archive::new(&raw[..]);
        ar.unpack(&o.outdir)
            .with_context(|| format!("cannot untar into {}", o.outdir.display()))?;
        eprintln!("extracted directory {}", dest.display());
    } else {
        std::fs::write(&dest, &raw).with_context(|| format!("cannot write {}", dest.display()))?;
        eprintln!("extracted file {}", dest.display());
    }
    Ok(())
}
