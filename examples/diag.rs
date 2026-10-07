// Diagnostic: scan a raw s16le/48kHz PCM file for FSK preamble candidates.
// Duplicates the Goertzel scorer from src/audio.rs (examples cannot see
// bin-crate internals).
use std::env;
use std::fs;

const SR: usize = 48000;
const SPB: usize = SR / 1200;
const TONE0: f64 = 1200.0;
const TONE1: f64 = 2400.0;

fn tables(freq: f64) -> (Vec<f32>, Vec<f32>) {
    let mut c = Vec::with_capacity(SPB);
    let mut s = Vec::with_capacity(SPB);
    for i in 0..SPB {
        let a = 2.0 * std::f64::consts::PI * freq * i as f64 / SR as f64;
        c.push(a.cos() as f32);
        s.push(a.sin() as f32);
    }
    (c, s)
}

fn score(x: &[f32], off: usize, lo: usize, hi: usize) -> (f32, f32) {
    let (c0, s0) = tables(TONE0);
    let (c1, s1) = tables(TONE1);
    let mut num = 0.0f32;
    let mut den = 1e-9f32;
    for k in lo..hi {
        let w = off + k * SPB;
        if w + SPB > x.len() {
            break;
        }
        let (mut r0, mut i0, mut r1, mut i1) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for j in 0..SPB {
            let v = x[w + j];
            r0 += v * c0[j];
            i0 += v * s0[j];
            r1 += v * c1[j];
            i1 += v * s1[j];
        }
        let (e0, e1) = (r0 * r0 + i0 * i0, r1 * r1 + i1 * i1);
        let d = e0 - e1;
        num += if k.is_multiple_of(2) { -d } else { d };
        den += e0 + e1;
    }
    (num / den, num)
}

fn main() {
    let path = env::args().nth(1).expect("pcm file");
    let data = fs::read(&path).unwrap();
    let pcm: Vec<i16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    let max = pcm.iter().map(|&s| s.abs()).max().unwrap();
    println!("samples {} max {}", pcm.len(), max);
    let scale = 1.0 / max as f32;
    let x: Vec<f32> = pcm.iter().map(|&s| s as f32 * scale).collect();

    // where does bit 1 of preamble look strong? (preamble_bit(1) = 0? bit k
    // alternates 1,0,1,0.. starting with k=0 -> 1)
    // use s8 over full track
    let mut hits: Vec<(usize, f32, f32)> = Vec::new();
    let max_start = x.len().saturating_sub(64 * SPB);
    let mut off = 0usize;
    while off <= max_start {
        let (s, n) = score(&x, off, 0, 8);
        if s > 0.5 {
            hits.push((off, s, n));
        }
        off += 10;
    }
    println!("coarse hits (s8 > 0.5): {}", hits.len());
    for &(o, s, n) in hits.iter().take(40) {
        let (s64, n64) = score(&x, o, 0, 64);
        println!("  off {o}: s8={s:.3} n8={n:.4} s64={s64:.3} n64={n64:.4}");
    }

    // fine scan of the first 6000 samples, best s64
    let mut best = (0usize, -1.0f32, -1.0f32);
    for o in 0..6000.min(max_start) {
        let (s64, n64) = score(&x, o, 0, 64);
        if n64 > best.2 {
            best = (o, s64, n64);
        }
    }
    println!(
        "best in 0..6000: off={} s64={:.3} n64={:.4}",
        best.0, best.1, best.2
    );
}
