// Measure per-cell BER between a reference frame and a (re-encoded) frame.
// Usage: ber ref.raw refW refH cur.raw curW curH
// Raw input is 8-bit gray. Grid: 240x135 cells, cell 8px, border 2,
// native 1920x1080.
use std::env;
use std::fs;

const COLS: usize = 240;
const ROWS: usize = 135;
const CELL: usize = 8;
const BORDER: usize = 2;
const NW: usize = COLS * CELL; // 1920
const NH: usize = ROWS * CELL; // 1080

fn median(buf: &mut [u8]) -> u8 {
    let n = buf.len();
    if n == 0 {
        return 0;
    }
    let (_, m, _) = buf.select_nth_unstable(n / 2);
    *m
}

struct Sampler {
    sx: f64,
    sy: f64,
    ox: i32,
    oy: i32,
    gw: usize,
    gh: usize,
}

impl Sampler {
    fn cell_median(&self, gray: &[u8], r: usize, c: usize) -> u8 {
        let cell = CELL as f64;
        let x0 = (self.ox as f64 + c as f64 * cell * self.sx).floor() as i64;
        let x1 = (self.ox as f64 + (c + 1) as f64 * cell * self.sx).ceil() as i64;
        let y0 = (self.oy as f64 + r as f64 * cell * self.sy).floor() as i64;
        let y1 = (self.oy as f64 + (r + 1) as f64 * cell * self.sy).ceil() as i64;
        let x0 = x0.max(0).min(self.gw as i64 - 1);
        let x1 = x1.max(x0 + 1).min(self.gw as i64);
        let y0 = y0.max(0).min(self.gh as i64 - 1);
        let y1 = y1.max(y0 + 1).min(self.gh as i64);
        let pad = |len: i64| -> i64 {
            let p = ((len as f64 * 0.2).round() as i64).max(1);
            p.min((len - 1) / 2)
        };
        let px = pad(x1 - x0);
        let py = pad(y1 - y0);
        let mut buf = Vec::new();
        for y in y0 + py..y1 - py {
            let row = &gray[y as usize * self.gw..(y as usize + 1) * self.gw];
            buf.extend_from_slice(&row[(x0 + px) as usize..(x1 - px) as usize]);
        }
        median(&mut buf)
    }
    fn border_score(&self, gray: &[u8]) -> f64 {
        let mut ok = 0usize;
        let mut total = 0usize;
        for r in 0..ROWS {
            for c in 0..COLS {
                let is_border =
                    !(BORDER..ROWS - BORDER).contains(&r) || !(BORDER..COLS - BORDER).contains(&c);
                if !is_border {
                    continue;
                }
                total += 1;
                let white = self.cell_median(gray, r, c) >= 128;
                if white == (r + c).is_multiple_of(2) {
                    ok += 1;
                }
            }
        }
        ok as f64 / total as f64
    }
    fn payload_bit(&self, gray: &[u8], r: usize, c: usize) -> bool {
        self.cell_median(gray, r, c) >= 128
    }
}

fn find_align(gray: &[u8], gw: usize, gh: usize) -> (i32, i32, f64) {
    let sx = gw as f64 / NW as f64;
    let sy = gh as f64 / NH as f64;
    let cell_x = CELL as f64 * sx;
    let cell_y = CELL as f64 * sy;
    let rx = cell_x.ceil() as i32 + 1;
    let ry = cell_y.ceil() as i32 + 1;
    let mut best = (0, 0, -1.0);
    for oy in -ry..=ry {
        for ox in -rx..=rx {
            let s = Sampler {
                sx,
                sy,
                ox,
                oy,
                gw,
                gh,
            };
            let v = s.border_score(gray);
            if v > best.2 {
                best = (ox, oy, v);
            }
        }
    }
    best
}

fn main() {
    let a = env::args().collect::<Vec<_>>();
    let (rp, rw, rh, cp, cw, ch) = (
        &a[1],
        a[2].parse::<usize>().unwrap(),
        a[3].parse::<usize>().unwrap(),
        &a[4],
        a[5].parse::<usize>().unwrap(),
        a[6].parse::<usize>().unwrap(),
    );
    let rdata = fs::read(rp).unwrap();
    let cdata = fs::read(cp).unwrap();
    assert_eq!(rdata.len() % (rw * rh), 0);
    assert_eq!(cdata.len() % (cw * ch), 0);

    let (rox, roy, rs) = find_align(&rdata, rw, rh);
    let (cox, coy, cs) = find_align(&cdata, cw, ch);
    println!("ref align ({rox},{roy}) border {rs:.3}; cur align ({cox},{coy}) border {cs:.3}");

    let rsmp = Sampler {
        sx: rw as f64 / NW as f64,
        sy: rh as f64 / NH as f64,
        ox: rox,
        oy: roy,
        gw: rw,
        gh: rh,
    };
    let csmp = Sampler {
        sx: cw as f64 / NW as f64,
        sy: ch as f64 / NH as f64,
        ox: cox,
        oy: coy,
        gw: cw,
        gh: ch,
    };

    let mut diff = 0usize;
    let mut total = 0usize;
    let mut frame_full = 0usize;
    let mut frames = 0usize;
    let nf = (rdata.len() / (rw * rh)).min(cdata.len() / (cw * ch));
    for f in 0..nf {
        let rfr = &rdata[f * rw * rh..][..rw * rh];
        let cfr = &cdata[f * cw * ch..][..cw * ch];
        let mut fdiff = 0usize;
        let mut ftot = 0usize;
        for r in BORDER..ROWS - BORDER {
            for c in BORDER..COLS - BORDER {
                if rsmp.payload_bit(rfr, r, c) != csmp.payload_bit(cfr, r, c) {
                    fdiff += 1;
                }
                ftot += 1;
            }
        }
        diff += fdiff;
        total += ftot;
        frames += 1;
        if fdiff == 0 {
            frame_full += 1;
        }
        if f < 3 || f == nf - 1 {
            println!("frame {f}: {fdiff}/{ftot} bits differ");
        }
    }
    println!(
        "overall BER: {diff}/{total} = {:.5}; exact frames: {frame_full}/{frames}",
        diff as f64 / total as f64
    );
}
