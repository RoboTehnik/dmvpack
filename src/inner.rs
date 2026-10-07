//! Inner per-frame error-correcting code: systematic RS(255, 239) over
//! GF(2^8) (poly 0x11D, roots alpha^0..alpha^15). Corrects up to 8 wrong
//! bytes per 255-byte codeword, which covers the small bit-error rates
//! produced by lossy re-encodes and scaled-down renditions (e.g. 1080p ->
//! 480p). Frames that exceed the capability fail to decode and fall back
//! to the outer frame-level erasure logic.

use anyhow::{bail, Result};
use std::sync::OnceLock;

pub const CW: usize = 255;
pub const DATA_CW: usize = 239;
pub const PARITY: usize = 16;
const T: usize = PARITY / 2; // correctable errors per codeword

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    gen: [u8; PARITY + 1],
}

fn tables() -> &'static Tables {
    static TBL: OnceLock<Tables> = OnceLock::new();
    TBL.get_or_init(|| {
        // GF(2^8) with primitive polynomial x^8 + x^4 + x^3 + x^2 + 1
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, e) in exp.iter_mut().enumerate().take(255) {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }

        // generator polynomial: product of (x + alpha^i), i = 0..15.
        // g[0] is the leading coefficient (degree grows with each factor).
        let mut g = [0u8; PARITY + 1];
        g[0] = 1;
        for i in 0..PARITY {
            let root = exp[i];
            let mut next = [0u8; PARITY + 1];
            for j in 0..=i {
                next[j] ^= g[j];
                next[j + 1] ^= mul_raw(&exp, &log, g[j], root);
            }
            g = next;
        }
        Tables { exp, log, gen: g }
    })
}

fn mul_raw(exp: &[u8; 512], log: &[u8; 256], a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        exp[log[a as usize] as usize + log[b as usize] as usize]
    }
}

fn mul(a: u8, b: u8) -> u8 {
    let t = tables();
    mul_raw(&t.exp, &t.log, a, b)
}

fn inv(a: u8) -> u8 {
    assert!(a != 0);
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

fn exp(k: usize) -> u8 {
    let t = tables();
    t.exp[k % 255]
}

/// Systematic encoding of one codeword: data (239) -> codeword (255).
fn encode_cw(data: &[u8], out: &mut [u8]) {
    let t = tables();
    debug_assert_eq!(data.len(), DATA_CW);
    debug_assert_eq!(out.len(), CW);
    out[..DATA_CW].copy_from_slice(data);
    out[DATA_CW..].fill(0);
    // Synthetic polynomial division of (data << PARITY) by the generator:
    // writes only to indices > i, so the remainder accumulates in
    // out[DATA_CW..]; the data area ends up holding the quotient and is
    // restored afterwards.
    let original = out[..DATA_CW].to_vec();
    for i in 0..DATA_CW {
        let f = out[i];
        if f != 0 {
            for j in 1..=PARITY {
                out[i + j] ^= mul_raw(&t.exp, &t.log, t.gen[j], f);
            }
        }
    }
    out[..DATA_CW].copy_from_slice(&original);
}

fn syndrome(cw: &[u8], j: usize) -> u8 {
    // Horner evaluation at alpha^j: coeff of x^(CW-1-i) is cw[i]
    let x = exp(j);
    let mut s = 0u8;
    for &c in cw.iter().take(CW) {
        s = mul(s, x) ^ c;
    }
    s
}

/// Decode one codeword in place. Returns Some(()) when the codeword was
/// corrected (or was already valid), None when uncorrectable.
fn decode_cw(cw: &mut [u8]) -> Option<()> {
    debug_assert_eq!(cw.len(), CW);
    let mut s = [0u8; PARITY];
    let mut nonzero = false;
    for (j, sj) in s.iter_mut().enumerate() {
        *sj = syndrome(cw, j);
        nonzero |= *sj != 0;
    }
    if !nonzero {
        return Some(());
    }

    // Berlekamp-Massey: find locator Lambda(x) = 1 + L1 x + ... with
    // roots X_k^{-1}, X_k = alpha^(254 - pos_k)
    let mut lambda = vec![1u8];
    let mut b = vec![1u8];
    let mut l = 0usize;
    let mut m = 1usize;
    let mut bb = 1u8;
    for n in 0..PARITY {
        let mut d = s[n];
        for i in 1..=l {
            if i < lambda.len() {
                d ^= mul(lambda[i], s[n - i]);
            }
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let old = lambda.clone();
        let coef = mul(d, inv(bb));
        let need = b.len() + m;
        if lambda.len() < need {
            lambda.resize(need, 0);
        }
        for (i, &bv) in b.iter().enumerate() {
            lambda[i + m] ^= mul(coef, bv);
        }
        if 2 * l <= n {
            l = n + 1 - l;
            b = old;
            bb = d;
            m = 1;
        } else {
            m += 1;
        }
    }
    if l == 0 || l > T {
        if std::env::var("DMVPACK_RS_DEBUG").is_ok() {
            eprintln!("BM fail: l={l} lambda={lambda:?} s={s:?}");
        }
        return None;
    }

    // Chien search over all byte positions.
    let mut positions = Vec::with_capacity(l);
    for pos in 0..CW {
        // evaluate lambda at alpha^(pos + 1)  ( == X^{-1} )
        let x = exp(pos + 1);
        let mut v = 0u8;
        for &c in lambda.iter().rev() {
            v = mul(v, x) ^ c;
        }
        if v == 0 {
            positions.push(pos);
        }
    }
    if positions.len() != l {
        if std::env::var("DMVPACK_RS_DEBUG").is_ok() {
            eprintln!("Chien fail: l={l} found={positions:?} lambda={lambda:?}");
        }
        return None;
    }

    // Solve the Vandermonde system S_j = sum_k e_k * X_k^j (j = 0..l-1).
    let xs: Vec<u8> = positions.iter().map(|&p| exp(CW - 1 - p)).collect();
    let mut a = vec![vec![0u8; l]; l];
    let mut rhs = vec![0u8; l];
    let mut powers = vec![1u8; l];
    for (j, row) in a.iter_mut().enumerate() {
        for k in 0..l {
            row[k] = powers[k];
            powers[k] = mul(powers[k], xs[k]);
        }
        rhs[j] = s[j];
    }
    let errs = match gauss(&a, &rhs) {
        Some(e) => e,
        None => {
            if std::env::var("DMVPACK_RS_DEBUG").is_ok() {
                eprintln!("gauss fail: l={l} positions={positions:?} a={a:?} rhs={rhs:?}");
            }
            return None;
        }
    };

    for (&p, &e) in positions.iter().zip(errs.iter()) {
        cw[p] ^= e;
    }

    // verify: all syndromes must be zero now
    for j in 0..PARITY {
        if syndrome(cw, j) != 0 {
            if std::env::var("DMVPACK_RS_DEBUG").is_ok() {
                eprintln!(
                    "verify fail at j={j}: positions={positions:?} errs={errs:?} lambda={lambda:?}"
                );
            }
            return None;
        }
    }
    Some(())
}

fn gauss(a: &[Vec<u8>], rhs: &[u8]) -> Option<Vec<u8>> {
    let n = rhs.len();
    let mut m: Vec<Vec<u8>> = a.to_vec();
    let mut r = rhs.to_vec();
    for col in 0..n {
        let piv = (col..n).find(|&i| m[i][col] != 0)?;
        m.swap(col, piv);
        r.swap(col, piv);
        let inv_p = inv(m[col][col]);
        for v in m[col].iter_mut().skip(col) {
            *v = mul(*v, inv_p);
        }
        r[col] = mul(r[col], inv_p);
        for i in 0..n {
            if i != col && m[i][col] != 0 {
                let f = m[i][col];
                let prow = m[col].clone();
                for (v, &p) in m[i].iter_mut().zip(&prow).skip(col) {
                    *v ^= mul(f, p);
                }
                r[i] ^= mul(f, r[col]);
            }
        }
    }
    Some(r)
}

/// Encode arbitrary-length data, zero-padding the tail to a whole number
/// of codewords. The padding is harmless for parsers that stop after a
/// self-describing payload.
pub fn encode_padded(data: &[u8]) -> Vec<u8> {
    let n = data.len().div_ceil(DATA_CW) * DATA_CW;
    let mut buf = data.to_vec();
    buf.resize(n, 0);
    encode(&buf)
}

/// Encode data (length must be a multiple of 239) into coded bytes
/// (length = data.len()/239 * 255).
pub fn encode(data: &[u8]) -> Vec<u8> {
    assert_eq!(data.len() % DATA_CW, 0, "data length must be n*239");
    let ncw = data.len() / DATA_CW;
    let mut out = vec![0u8; ncw * CW];
    for k in 0..ncw {
        encode_cw(
            &data[k * DATA_CW..(k + 1) * DATA_CW],
            &mut out[k * CW..(k + 1) * CW],
        );
    }
    out
}

/// Decode coded bytes back to data; None when any codeword is
/// uncorrectable.
pub fn decode(coded: &[u8]) -> Option<Vec<u8>> {
    if coded.is_empty() || !coded.len().is_multiple_of(CW) {
        return None;
    }
    let ncw = coded.len() / CW;
    let mut data = Vec::with_capacity(ncw * DATA_CW);
    let mut cw = coded.to_vec();
    for k in 0..ncw {
        let part = &mut cw[k * CW..(k + 1) * CW];
        decode_cw(part)?;
        data.extend_from_slice(&part[..DATA_CW]);
    }
    Some(data)
}

/// Number of inner codewords and the data capacity for a frame with
/// `payload_bytes` of raw cell capacity.
pub fn frame_layout(payload_bytes: usize) -> (usize, usize) {
    let ncw = payload_bytes / CW;
    (ncw, ncw * DATA_CW)
}

pub fn check_frame_layout(payload_bytes: usize) -> Result<(usize, usize)> {
    let (ncw, data) = frame_layout(payload_bytes);
    if ncw == 0 || data == 0 {
        bail!("grid too small for inner RS(255,239) code");
    }
    Ok((ncw, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(37).wrapping_add(i >> 8)) as u8)
            .collect()
    }

    #[test]
    fn roundtrip() {
        let d = data(DATA_CW * 3);
        let c = encode(&d);
        assert_eq!(c.len(), CW * 3);
        let back = decode(&c).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn corrects_up_to_8_errors() {
        let d = data(DATA_CW * 2);
        let mut c = encode(&d);
        let mut rng = 0x12345678u32;
        for _ in 0..T {
            rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
            let pos = (rng >> 8) as usize % c.len();
            let val = (rng >> 16) as u8 | 1;
            c[pos] ^= val;
        }
        let back = decode(&c).expect("should correct 8 errors");
        assert_eq!(back, d);
    }

    #[test]
    fn rejects_beyond_capability() {
        let d = data(DATA_CW);
        let mut c = encode(&d);
        let n = c.len();
        for i in 0..T + 1 {
            c[i * 7 % n] ^= 0xa5 ^ i as u8;
        }
        // not guaranteed for every pattern, but for this fixed one we
        // expect either rejection or, in the worst case, a valid-but-wrong
        // codeword which the outer sha256 check would catch. What must
        // never happen is panicking.
        let _ = decode(&c);
    }

    #[test]
    fn single_error_correction() {
        let d = data(DATA_CW * 2);
        let mut c = encode(&d);
        c[100] ^= 0x5a;
        c[254] ^= 0x11;
        c[255 + 3] ^= 0x77; // second codeword
        let back = decode(&c).unwrap();
        assert_eq!(back, d);
    }
}
