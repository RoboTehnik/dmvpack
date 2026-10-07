use crate::inner;
use crate::manifest::Manifest;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;

pub const SAMPLE_RATE: usize = 48000;
pub const BAUD: usize = 1200;
pub const SPB: usize = SAMPLE_RATE / BAUD; // samples per symbol
pub const TONE0: f64 = 1200.0; // bit 0
pub const TONE1: f64 = 2400.0; // bit 1
pub const PREAMBLE_BITS: usize = 256;
pub const GAP_SAMPLES: usize = SAMPLE_RATE / 20; // 50 ms between copies
const AMPLITUDE: f64 = 0.65;
const MAX_COPIES: usize = 16;

/// Pseudo-random preamble (sha256 of a fixed string, 256 bits). An
/// alternating 1,0,1,0... pattern is ambiguous up to a 2-bit shift, which
/// makes byte alignment fail after lossy re-encoding; a random-looking
/// pattern has a sharp autocorrelation peak only at the exact offset.
fn preamble_digest() -> &'static [u8; 32] {
    static D: OnceLock<[u8; 32]> = OnceLock::new();
    D.get_or_init(|| {
        let mut h = Sha256::new();
        h.update(b"vidarc-preamble-v1");
        h.finalize().into()
    })
}

fn preamble_bit(i: usize) -> u8 {
    let d = preamble_digest();
    (d[i / 8] >> (7 - i % 8)) & 1
}

struct ToneTables {
    c0: Vec<f32>,
    s0: Vec<f32>,
    c1: Vec<f32>,
    s1: Vec<f32>,
}

impl ToneTables {
    fn new() -> Self {
        let mut c0 = Vec::with_capacity(SPB);
        let mut s0 = Vec::with_capacity(SPB);
        let mut c1 = Vec::with_capacity(SPB);
        let mut s1 = Vec::with_capacity(SPB);
        for i in 0..SPB {
            let a0 = 2.0 * std::f64::consts::PI * TONE0 * i as f64 / SAMPLE_RATE as f64;
            let a1 = 2.0 * std::f64::consts::PI * TONE1 * i as f64 / SAMPLE_RATE as f64;
            c0.push(a0.cos() as f32);
            s0.push(a0.sin() as f32);
            c1.push(a1.cos() as f32);
            s1.push(a1.sin() as f32);
        }
        ToneTables { c0, s0, c1, s1 }
    }
}

/// Energy of `window` at tone 0 / tone 1 (correlation magnitude squared).
fn energies(x: &[f32], off: usize, t: &ToneTables) -> (f32, f32) {
    let w = &x[off..off + SPB];
    let (mut r0, mut i0, mut r1, mut i1) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for (j, &v) in w.iter().enumerate() {
        r0 += v * t.c0[j];
        i0 += v * t.s0[j];
        r1 += v * t.c1[j];
        i1 += v * t.s1[j];
    }
    (r0 * r0 + i0 * i0, r1 * r1 + i1 * i1)
}

/// Score over bits [lo, hi) of the preamble assuming it starts at `off`.
/// Returns (num / den) in roughly [-1, 1] and the raw numerator `num`
/// (energy-weighted confidence, used to pick full-coverage offsets).
fn preamble_score(x: &[f32], off: usize, t: &ToneTables, lo: usize, hi: usize) -> (f32, f32) {
    let mut num = 0.0f32;
    let mut den = 1e-9f32;
    for k in lo..hi {
        let woff = off + k * SPB;
        if woff + SPB > x.len() {
            break;
        }
        let (e0, e1) = energies(x, woff, t);
        let d = e0 - e1;
        num += if preamble_bit(k) == 0 { d } else { -d };
        den += e0 + e1;
    }
    (num / den, num)
}

fn emit_symbol(out: &mut Vec<i16>, bit: u8, phase: &mut f64) {
    let freq = if bit == 0 { TONE0 } else { TONE1 };
    let step = 2.0 * std::f64::consts::PI * freq / SAMPLE_RATE as f64;
    let amp = AMPLITUDE * i16::MAX as f64;
    for _ in 0..SPB {
        *phase += step;
        if *phase > std::f64::consts::PI {
            *phase -= 2.0 * std::f64::consts::PI;
        }
        out.push((amp * phase.sin()).round() as i16);
    }
}

fn emit_bits(out: &mut Vec<i16>, get_bit: impl Fn(usize) -> u8, nbits: usize, phase: &mut f64) {
    for i in 0..nbits {
        emit_symbol(out, get_bit(i), phase);
    }
}

/// Samples needed to fit `copies` full [preamble][blob] copies (with gaps).
pub fn min_samples_for(blob_bytes: usize, copies: usize) -> usize {
    let coded = blob_bytes.div_ceil(inner::DATA_CW) * inner::CW;
    let copy = PREAMBLE_BITS + coded * 8;
    copies * (copy * SPB + GAP_SAMPLES) - GAP_SAMPLES
}

/// Generate the full audio track (length `total_samples`) carrying `blob`
/// as repeated [preamble][RS(blob)] copies. The blob is protected with the
/// inner RS(255,239) code so a few corrupted symbols after a lossy audio
/// re-encode do not destroy the manifest.
pub fn modulate_samples(blob: &[u8], total_samples: usize) -> Result<Vec<i16>> {
    let coded = inner::encode_padded(blob);
    let pre_samples = PREAMBLE_BITS * SPB;
    let blob_samples = coded.len() * 8 * SPB;
    let copy = pre_samples + blob_samples;
    if copy == 0 {
        bail!("empty manifest");
    }
    let mut copies = (total_samples + GAP_SAMPLES) / (copy + GAP_SAMPLES);
    if copies == 0 {
        bail!(
            "manifest ({} bytes) does not fit into the audio track ({} samples)",
            blob.len(),
            total_samples
        );
    }
    copies = copies.min(MAX_COPIES);

    let mut out: Vec<i16> = Vec::with_capacity(total_samples);
    let mut phase = 0.0f64;
    for c in 0..copies {
        emit_bits(&mut out, preamble_bit, PREAMBLE_BITS, &mut phase);
        emit_bits(
            &mut out,
            |i| (coded[i / 8] >> (7 - i % 8)) & 1,
            coded.len() * 8,
            &mut phase,
        );
        if c + 1 < copies {
            out.extend(std::iter::repeat_n(0i16, GAP_SAMPLES));
            phase = 0.0;
        }
    }
    out.resize(total_samples, 0);
    Ok(out)
}

/// Write the audio track as a mono 16-bit WAV file.
pub fn write_wav(path: &Path, blob: &[u8], total_samples: usize) -> Result<()> {
    let samples = modulate_samples(blob, total_samples)?;
    let data_len = (samples.len() * 2) as u32;
    let mut buf: Vec<u8> = Vec::with_capacity(44 + samples.len() * 2);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    buf.extend_from_slice(&((SAMPLE_RATE * 2) as u32).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, buf).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

fn try_parse_stream(bytes: &[u8]) -> Option<Manifest> {
    let mut start = 0usize;
    while start + 4 <= bytes.len() {
        if &bytes[start..start + 4] == b"VARC" {
            if let Ok(m) = Manifest::from_bytes(&bytes[start..]) {
                return Some(m);
            }
        }
        start += 1;
    }
    None
}

/// Turn one candidate's raw bit stream into a manifest: skip the preamble
/// bytes, then try RS block counts from longest to shortest (the tail of
/// the stream is silence/garbage after the real blob).
fn try_decode_stream(bytes: &[u8]) -> Option<Manifest> {
    const START: usize = PREAMBLE_BITS / 8;
    if bytes.len() <= START + inner::CW {
        return None;
    }
    let blob = &bytes[START..];
    let max_blocks = blob.len() / inner::CW;
    for nb in (1..=max_blocks).rev() {
        if let Some(dec) = inner::decode(&blob[..nb * inner::CW]) {
            if let Some(m) = try_parse_stream(&dec) {
                return Some(m);
            }
        }
    }
    None
}

fn normalize(pcm: &[i16]) -> Result<Vec<f32>> {
    if pcm.len() < SPB * 64 {
        bail!("audio track too short");
    }
    let max = pcm.iter().map(|&s| s.abs()).max().unwrap_or(0);
    if max < 500 {
        bail!("audio track is silent (manifest lost)");
    }
    let scale = 1.0 / max as f32;
    Ok(pcm.iter().map(|&s| s as f32 * scale).collect())
}

/// Locate preamble candidates in a normalized signal.
fn find_candidates(x: &[f32]) -> Vec<usize> {
    let t = ToneTables::new();
    let n = x.len();
    let max_start = n.saturating_sub(PREAMBLE_BITS * SPB);
    let step = (SPB / 4).max(1);

    // coarse scan: first 8 preamble bits only
    let mut refined: Vec<(usize, f32, f32)> = Vec::new(); // (offset, score, num)
    let mut off = 0usize;
    while off <= max_start {
        let (s, _) = preamble_score(x, off, &t, 0, 8);
        if s > 0.20 {
            let mut best = (off, -1.0f32, -1.0f32);
            let lo = off.saturating_sub(step);
            let hi = (off + step).min(max_start);
            let mut o = lo;
            while o <= hi {
                let (s64, n64) = preamble_score(x, o, &t, 0, 64);
                if s64 > 0.55 && n64 > best.2 {
                    best = (o, s64, n64);
                }
                o += 1;
            }
            if best.2 >= 0.0 {
                refined.push(best);
            }
        }
        off += step;
    }

    // Cluster candidates by position; inside a cluster prefer the largest
    // raw numerator (full signal coverage) — near-preamble offsets that
    // start in silence can tie the normalized score with the exact offset
    // because silent windows contribute 0/0.
    refined.sort_by_key(|&(o, _, _)| o);
    let nref = refined.len();
    let mut clusters: Vec<(usize, usize, f32, f32)> = Vec::new(); // (last_o, best_o, best_num, best_score)
    for (o, s, nv) in refined {
        match clusters.last_mut() {
            Some(last) if o - last.0 <= PREAMBLE_BITS * SPB => {
                last.0 = o;
                if nv > last.2 {
                    last.1 = o;
                    last.2 = nv;
                    last.3 = s;
                }
            }
            _ => clusters.push((o, o, nv, s)),
        }
    }
    clusters.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    let mut chosen: Vec<usize> = clusters.iter().take(64).map(|c| c.1).collect();
    chosen.sort_unstable();

    if std::env::var("VIDARC_DEBUG").is_ok() {
        eprintln!(
            "candidates: {chosen:?} (refined {nref}, clusters {})",
            clusters.len()
        );
    }
    chosen
}

/// Hard bit decisions from `start` to the end of the signal.
fn hard_bits(x: &[f32], start: usize, t: &ToneTables) -> Vec<u8> {
    let n = x.len();
    let mut bits = Vec::with_capacity((n - start) / SPB);
    let mut w = start;
    while w + SPB <= n {
        let (e0, e1) = energies(x, w, t);
        bits.push(if e1 > e0 { 1u8 } else { 0u8 });
        w += SPB;
    }
    bits
}

/// Extract the raw byte stream (preamble + payload) at a candidate offset.
fn read_candidate(x: &[f32], cand: usize, t: &ToneTables) -> Vec<u8> {
    let bits = hard_bits(x, cand, t);
    let nbytes = bits.len() / 8;
    let mut bytes = Vec::with_capacity(nbytes);
    for i in 0..nbytes {
        let mut b = 0u8;
        for j in 0..8 {
            b |= bits[i * 8 + j] << (7 - j);
        }
        bytes.push(b);
    }
    bytes
}

/// Does this candidate show at least one decodable RS block? (A real copy
/// damaged beyond repair still shows partial structure; pure noise does not.)
fn plausible(x: &[f32], cand: usize, t: &ToneTables) -> bool {
    let bytes = read_candidate(x, cand, t);
    let blob = &bytes[PREAMBLE_BITS / 8..];
    let nb = blob.len() / inner::CW;
    (0..nb).any(|k| inner::decode(&blob[k * inner::CW..(k + 1) * inner::CW]).is_some())
}

/// Align every candidate to `anchor`'s bit grid, soft-combine their tone
/// energies per bit and try to decode. The combined stream is correct
/// whenever the copies are not wrong in the same place, which cuts the bit
/// error rate dramatically (errors of independent copies rarely coincide).
fn combine_once(
    x: &[f32],
    candidates: &[usize],
    anchor: usize,
    t: &ToneTables,
    dbg: bool,
) -> Option<Manifest> {
    let n = x.len();
    let anchor_bits = hard_bits(x, anchor, t);
    let ab = PREAMBLE_BITS;
    let cmp_bits = 1024.min(anchor_bits.len().saturating_sub(ab));
    if cmp_bits < 64 {
        return None;
    }

    // residual of each copy relative to the anchor grid; real copies sit a
    // whole number of symbols apart, foreign noise does not agree on payload
    let mut aligned: Vec<(usize, i32)> = vec![(anchor, 0)];
    for &c in candidates {
        if c == anchor {
            continue;
        }
        let mut best: Option<(i32, usize)> = None;
        for e in -20i32..=20 {
            let s = c as i64 + e as i64;
            if s < 0 {
                continue;
            }
            let bits = hard_bits(x, s as usize, t);
            let agree = (0..cmp_bits)
                .filter(|&j| bits[ab + j] == anchor_bits[ab + j])
                .count();
            if best.map(|(_, a)| agree > a).unwrap_or(true) {
                best = Some((e, agree));
            }
        }
        if let Some((e, agree)) = best {
            // 65%: real copies agree far above this even on heavily
            // re-encoded tracks, random noise sits near 50%
            if agree * 100 >= cmp_bits * 65 {
                if dbg {
                    eprintln!(
                        "combine(anchor {anchor}): cand {c} residual {e}, agree {agree}/{cmp_bits}"
                    );
                }
                aligned.push((c, e));
            }
        }
    }
    if aligned.len() < 2 {
        return None;
    }

    // soft-combine: sum tone energies per bit over the aligned grids
    // (starting at the preamble so the byte layout matches the strict path)
    if anchor >= n {
        return None;
    }
    let total_bits = (n - anchor) / SPB;
    let mut bits = Vec::with_capacity(total_bits);
    for j in 0..total_bits {
        let mut s0 = 0f32;
        let mut s1 = 0f32;
        for &(c, e) in &aligned {
            let w = (c as i64 + e as i64 + (j * SPB) as i64).max(0) as usize;
            if w + SPB <= n {
                let (e0, e1) = energies(x, w, t);
                s0 += e0;
                s1 += e1;
            }
        }
        bits.push(if s1 > s0 { 1u8 } else { 0u8 });
    }

    let nbytes = bits.len() / 8;
    let mut bytes = Vec::with_capacity(nbytes);
    for i in 0..nbytes {
        let mut b = 0u8;
        for j in 0..8 {
            b |= bits[i * 8 + j] << (7 - j);
        }
        bytes.push(b);
    }
    if dbg {
        let b = &bytes[PREAMBLE_BITS / 8..];
        let nb = b.len() / inner::CW;
        let st: Vec<bool> = (0..nb)
            .map(|k| inner::decode(&b[k * inner::CW..(k + 1) * inner::CW]).is_some())
            .collect();
        eprintln!(
            "combined {} copies (anchor {anchor}) -> {} bytes, per-block ok {st:?}",
            aligned.len(),
            bytes.len()
        );
    }
    try_decode_stream(&bytes)
}

/// When no single copy survives the RS check (e.g. a heavily re-encoded
/// audio track), combine the copies. Candidates that show RS structure are
/// tried as anchors first.
fn try_combine(x: &[f32], candidates: &[usize], t: &ToneTables) -> Option<Manifest> {
    let dbg = std::env::var("VIDARC_DEBUG").is_ok();
    if candidates.len() < 2 {
        if dbg {
            eprintln!("combine: only {} candidate(s)", candidates.len());
        }
        return None;
    }
    let mut anchors: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&c| plausible(x, c, t))
        .collect();
    for &c in candidates {
        if !anchors.contains(&c) {
            anchors.push(c);
        }
    }
    if dbg {
        eprintln!("combine: anchors {anchors:?}");
    }
    for anchor in anchors {
        if let Some(m) = combine_once(x, candidates, anchor, t, dbg) {
            return Some(m);
        }
    }
    None
}

/// Demodulate the audio track and extract the embedded manifest.
pub fn demodulate(pcm: &[i16]) -> Result<Manifest> {
    let x = normalize(pcm)?;
    let chosen = find_candidates(&x);
    if chosen.is_empty() {
        bail!("no manifest preamble found in audio track");
    }

    let t = ToneTables::new();
    for &cand in &chosen {
        let bytes = read_candidate(&x, cand, &t);
        if let Some(m) = try_decode_stream(&bytes) {
            return Ok(m);
        }
        if std::env::var("VIDARC_DEBUG").is_ok() {
            let b = &bytes[PREAMBLE_BITS / 8..];
            let nblocks = b.len() / inner::CW;
            let st: Vec<bool> = (0..nblocks)
                .map(|k| inner::decode(&b[k * inner::CW..(k + 1) * inner::CW]).is_some())
                .collect();
            eprintln!("cand {cand}: {} bytes, per-block ok {st:?}", bytes.len());
        }
    }
    if let Some(m) = try_combine(&x, &chosen, &t) {
        return Ok(m);
    }
    bail!("manifest not found or corrupted in audio track")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{EccSpec, GridSpec, Manifest, PayloadSpec, VideoSpec};

    fn sample_manifest() -> Manifest {
        Manifest {
            version: 1,
            grid: GridSpec {
                cols: 240,
                rows: 135,
                cell: 8,
                border: 2,
            },
            ecc: EccSpec {
                group_frames: 4,
                parity_frames: 1,
                shard_bytes: 3864,
            },
            video: VideoSpec {
                width: 1920,
                height: 1080,
                fps: 30,
                crf: 16,
                frames: 416,
            },
            payload: PayloadSpec {
                raw_len: 1_258_292,
                raw_sha256: "0123456789abcdef".repeat(4),
                compressed_len: 987_654,
                groups: 104,
                source_name: "sample.bin".into(),
                source_is_dir: false,
            },
            frame_crc: (0..416u32)
                .map(|i| (i.wrapping_mul(2654435761) >> 16) as u16)
                .collect(),
        }
    }

    #[test]
    fn fsk_roundtrip() {
        let m = sample_manifest();
        let total = 48000 * 15;
        let pcm = modulate_samples(&m.to_bytes(), total).unwrap();
        let got = demodulate(&pcm).unwrap_or_else(|e| panic!("demod failed: {e}"));
        assert_eq!(got, m);
    }

    #[test]
    fn fsk_roundtrip_with_leading_silence() {
        let m = sample_manifest();
        let total = 48000 * 15;
        let mut pcm = modulate_samples(&m.to_bytes(), total).unwrap();
        let mut lead = vec![0i16; 12345];
        lead.append(&mut pcm);
        lead.truncate(total);
        let got = demodulate(&lead).unwrap_or_else(|e| panic!("demod failed: {e}"));
        assert_eq!(got, m);
    }

    #[test]
    fn fsk_roundtrip_first_copy_damaged() {
        let m = sample_manifest();
        // 20s guarantees two manifest copies even after RS expansion
        let total = 48000 * 20;
        let mut pcm = modulate_samples(&m.to_bytes(), total).unwrap();
        // destroy the first ~3 seconds with noise; second copy must still decode
        for (i, s) in pcm.iter_mut().take(48000 * 3).enumerate() {
            *s = (i as i16).wrapping_mul(12345) & 0x3fff;
        }
        let got = demodulate(&pcm).unwrap_or_else(|e| panic!("demod failed: {e}"));
        assert_eq!(got, m);
    }

    /// Manual diagnostic: per-block error counts of an FSK stream against
    /// the reference coded blob. Run with:
    /// `VIDARC_DIAG_PCM=<raw.s16le> VIDARC_DIAG_REF=<ref.mp4> \
    ///  cargo test --release audio_damage -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn audio_damage() {
        let pcm_path = std::env::var("VIDARC_DIAG_PCM").expect("VIDARC_DIAG_PCM");
        let ref_path = std::env::var("VIDARC_DIAG_REF").expect("VIDARC_DIAG_REF");
        let raw = std::fs::read(&pcm_path).unwrap();
        let pcm: Vec<i16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();

        let ref_pcm = crate::ffmpeg::extract_audio_pcm(std::path::Path::new(&ref_path)).unwrap();
        let man = demodulate(&ref_pcm).unwrap();
        let expected = inner::encode_padded(&man.to_bytes());

        let x = normalize(&pcm).unwrap();
        let chosen = find_candidates(&x);
        let t = ToneTables::new();
        eprintln!(
            "expected blob: {} bytes ({} blocks)",
            expected.len(),
            expected.len() / inner::CW
        );
        for cand in &chosen {
            let bytes = read_candidate(&x, *cand, &t);
            let blob = &bytes[PREAMBLE_BITS / 8..];
            let nblocks = blob.len() / inner::CW;
            let mut report = Vec::new();
            for k in 0..nblocks {
                let got = &blob[k * inner::CW..(k + 1) * inner::CW];
                if (k + 1) * inner::CW <= expected.len() {
                    let want = &expected[k * inner::CW..(k + 1) * inner::CW];
                    let errs: Vec<usize> = got
                        .iter()
                        .zip(want)
                        .enumerate()
                        .filter(|(_, (g, w))| *g != *w)
                        .map(|(i, _)| i)
                        .collect();
                    let pos: Vec<usize> = errs.iter().take(8).copied().collect();
                    report.push(format!("{}@{pos:?}", errs.len()));
                }
            }
            eprintln!(
                "cand {cand}: {} bytes; per-block errors (count) {report:?}",
                bytes.len()
            );
        }

        // combined analysis over every residual of the second candidate
        if chosen.len() >= 2 {
            let n = x.len();
            let anchor = chosen[0];
            let c2 = chosen[1];
            let anchor_bits = hard_bits(&x, anchor, &t);
            let ab = PREAMBLE_BITS;
            let cmp = 1024.min(anchor_bits.len().saturating_sub(ab));
            let total = (n.saturating_sub(anchor + PREAMBLE_BITS * SPB)) / SPB;
            for e in -20i32..=20 {
                let s = c2 as i64 + e as i64;
                if s < 0 {
                    continue;
                }
                let s = s as usize;
                let bits2 = hard_bits(&x, s, &t);
                let agree = (0..cmp.min(bits2.len().saturating_sub(ab)))
                    .filter(|&j| bits2[ab + j] == anchor_bits[ab + j])
                    .count();
                // soft combine
                let mut errs = vec![0usize; expected.len() / inner::CW];
                for j in 0..total * 8 {
                    let mut s0 = 0f32;
                    let mut s1 = 0f32;
                    let wa = anchor + (PREAMBLE_BITS + j) * SPB;
                    if wa + SPB <= n {
                        let (a0, a1) = energies(&x, wa, &t);
                        s0 += a0;
                        s1 += a1;
                    }
                    let wb = s + (PREAMBLE_BITS + j) * SPB;
                    if wb + SPB <= n {
                        let (b0, b1) = energies(&x, wb, &t);
                        s0 += b0;
                        s1 += b1;
                    }
                    let bit = if s1 > s0 { 1u8 } else { 0u8 };
                    let byte = j / 8;
                    if byte < expected.len() && bit != ((expected[byte] >> (7 - j % 8)) & 1) {
                        errs[byte / inner::CW] += 1;
                    }
                }
                let tot: usize = errs.iter().sum();
                eprintln!("e={e}: agree={agree}/{cmp} total_errs={tot} per_block={errs:?}");
            }
        }
    }
}
