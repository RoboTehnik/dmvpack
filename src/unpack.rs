use crate::ecc;
use crate::ffmpeg;
use crate::frame::{find_alignment, payload_bytes, Sampler};
use crate::inner;
use crate::manifest::{Manifest, PartSpec};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

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
    /// One video per part (a single file for an unsplit archive).
    pub videos: Vec<PathBuf>,
    pub manifest: Option<PathBuf>,
    pub outdir: PathBuf,
    /// Password for encrypted archives (resolved, may come from the env).
    pub password: Option<String>,
}

/// Recover the manifest of one video: from JSON (override) or its audio track.
fn read_manifest(video: &Path, manifest_override: Option<&Path>) -> Result<Manifest> {
    ffmpeg::check_video(video)?;
    match manifest_override {
        Some(p) => {
            eprintln!("manifest override: {}", p.display());
            Manifest::load(p)
        }
        None => {
            let t0 = std::time::Instant::now();
            let pcm = ffmpeg::extract_audio_pcm(video)?;
            eprintln!(
                "  audio: {} samples ({:.1}s) [{:.1}s]",
                pcm.len(),
                pcm.len() as f64 / crate::audio::SAMPLE_RATE as f64,
                t0.elapsed().as_secs_f64()
            );
            let man = crate::audio::demodulate(&pcm)?;
            eprintln!(
                "  manifest recovered from audio ({} bytes) [{:.1}s]",
                man.to_bytes().len(),
                t0.elapsed().as_secs_f64()
            );
            Ok(man)
        }
    }
}

/// Decode one video into its slice of the compressed payload.
fn decode_payload(video: &Path, man: &Manifest) -> Result<Vec<u8>> {
    let (gw, gh) = ffmpeg::probe_size(video)?;
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
    let slice_len = man
        .part
        .map(|p| p.payload_len)
        .unwrap_or(man.payload.compressed_len) as usize;
    eprintln!(
        "video {} ({}x{}), expecting {} frames, {} bytes/frame, {} payload bytes",
        video.display(),
        gw,
        gh,
        e,
        man.ecc.shard_bytes,
        slice_len
    );

    // ---- pass A: per-frame CRCs over the whole video ----
    let t_start = std::time::Instant::now();
    let mut alignment: Option<(i32, i32, f64)> = None;
    let mut video_crc: Vec<Option<u16>> = Vec::new();
    let mut inner_ok = 0usize;
    let mut tried = 0usize;
    {
        let mut rd = ffmpeg::FrameReader::open(video, gw, gh)?;
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
        let mut rd = ffmpeg::FrameReader::open(video, gw, gh)?;
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
    let mut data = Vec::with_capacity(slice_len);
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
    if data.len() < slice_len {
        bail!(
            "part payload short: assembled {} bytes, need {}",
            data.len(),
            slice_len
        );
    }
    data.truncate(slice_len);
    Ok(data)
}

/// Check that every part describes the same archive.
/// Returns video indices ordered by part index.
fn check_parts(mans: &[Manifest], videos: &[PathBuf]) -> Result<Vec<usize>> {
    let base = &mans[0];
    for (m, v) in mans.iter().zip(videos) {
        if m.payload.raw_sha256 != base.payload.raw_sha256
            || m.payload.raw_len != base.payload.raw_len
            || m.payload.compressed_len != base.payload.compressed_len
            || m.payload.source_name != base.payload.source_name
            || m.payload.source_is_dir != base.payload.source_is_dir
            || m.grid != base.grid
            || m.ecc != base.ecc
            || m.encryption != base.encryption
        {
            bail!(
                "{}: manifest does not match the first video \
                 (different archive or wrong file order)",
                v.display()
            );
        }
    }

    let mut order: Vec<usize> = (0..mans.len()).collect();
    order.sort_by_key(|&i| mans[i].part.map(|p| p.index).unwrap_or(0));

    let mut expect = 0u64;
    for (pos, &i) in order.iter().enumerate() {
        let p = mans[i].part.unwrap_or(PartSpec {
            index: 0,
            payload_offset: 0,
            payload_len: base.payload.compressed_len,
        });
        if p.index as usize != pos {
            let have: Vec<String> = order
                .iter()
                .map(|&j| mans[j].part.map(|q| q.index).unwrap_or(0).to_string())
                .collect();
            bail!("part {pos} is missing (have parts {})", have.join(", "));
        }
        if p.payload_offset != expect {
            bail!(
                "part {} starts at offset {}, expected {}",
                p.index,
                p.payload_offset,
                expect
            );
        }
        expect += p.payload_len;
    }
    if expect != base.payload.compressed_len {
        bail!(
            "parts cover {expect} payload bytes, archive has {} \
             (a part file is missing?)",
            base.payload.compressed_len
        );
    }
    Ok(order)
}

pub fn run(o: UnpackOpts) -> Result<()> {
    let t_all = std::time::Instant::now();
    if o.videos.is_empty() {
        bail!("no input videos");
    }
    if o.manifest.is_some() && o.videos.len() > 1 {
        bail!("manifest override (-m) works only with a single video");
    }

    // ---- read all manifests ----
    let mut mans = Vec::with_capacity(o.videos.len());
    for v in &o.videos {
        mans.push(read_manifest(v, o.manifest.as_deref())?);
    }

    // ---- validate the set ----
    let order = check_parts(&mans, &o.videos)?;
    let mut base = mans[0].clone();
    let stream_len = base.payload.compressed_len;

    // ---- decode every part in order ----
    let mut data = Vec::with_capacity(stream_len as usize);
    for &i in &order {
        let slice = decode_payload(&o.videos[i], &mans[i])?;
        if let Some(p) = mans[i].part {
            if slice.len() as u64 != p.payload_len {
                bail!(
                    "{}: decoded {} bytes, manifest says {}",
                    o.videos[i].display(),
                    slice.len(),
                    p.payload_len
                );
            }
        }
        data.extend_from_slice(&slice);
    }
    if data.len() as u64 != stream_len {
        bail!(
            "assembled {} payload bytes, expected {}",
            data.len(),
            stream_len
        );
    }

    // ---- decrypt (password-protected archives) ----
    if let Some(spec) = base.encryption.clone() {
        let pw = match o.password.as_deref() {
            Some(p) => p,
            None => bail!("archive is encrypted: pass --password or set DMVPACK_PASSWORD"),
        };
        let sealed = crate::crypt::decrypt(&data, &spec, pw)?;
        let (zs, pj) = crate::crypt::unwrap_sealed(&sealed)?;
        let priv_spec: crate::crypt::PrivateSpec =
            serde_json::from_slice(pj).context("corrupt private manifest")?;
        data = zs.to_vec();
        base.payload.raw_len = priv_spec.raw_len;
        base.payload.raw_sha256 = priv_spec.raw_sha256;
        base.payload.compressed_len = priv_spec.compressed_len;
        base.payload.source_name = priv_spec.source_name;
        base.payload.source_is_dir = priv_spec.source_is_dir;
        eprintln!(
            "decrypted: {} payload bytes (argon2id + chacha20-poly1305) [{:.1}s]",
            data.len(),
            t_all.elapsed().as_secs_f64()
        );
    }

    eprintln!(
        "archive: {} ({} bytes raw, {} compressed, {} part(s))",
        base.payload.source_name,
        base.payload.raw_len,
        base.payload.compressed_len,
        order.len()
    );

    // ---- decompress ----
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&data[..])
        .map_err(|e| anyhow::anyhow!("zstd frame init: {e}"))?;
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| anyhow::anyhow!("zstd decompress: {e}"))?;

    // ---- integrity ----
    let got_sha = sha256_hex(&raw);
    if got_sha != base.payload.raw_sha256 {
        bail!(
            "payload checksum mismatch: got {got_sha}, expected {}",
            base.payload.raw_sha256
        );
    }
    if raw.len() as u64 != base.payload.raw_len {
        bail!(
            "payload length mismatch: got {}, expected {}",
            raw.len(),
            base.payload.raw_len
        );
    }
    eprintln!("payload verified: {} bytes, sha256 ok", raw.len());

    // ---- extract ----
    std::fs::create_dir_all(&o.outdir)
        .with_context(|| format!("cannot create {}", o.outdir.display()))?;
    let dest = o.outdir.join(&base.payload.source_name);
    if base.payload.source_is_dir {
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
