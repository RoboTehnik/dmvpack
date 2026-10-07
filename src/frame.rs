use crate::manifest::GridSpec;

pub const WHITE: u8 = 255;
pub const BLACK: u8 = 0;

pub fn native_size(g: &GridSpec) -> (usize, usize) {
    (g.cols * g.cell, g.rows * g.cell)
}

pub fn inner_dims(g: &GridSpec) -> (usize, usize) {
    (g.cols - 2 * g.border, g.rows - 2 * g.border)
}

pub fn payload_bytes(g: &GridSpec) -> usize {
    let (iw, ih) = inner_dims(g);
    (iw * ih) / 8
}

fn border_expected(r: usize, c: usize) -> bool {
    (r + c).is_multiple_of(2)
}

pub fn render_frame(g: &GridSpec, payload: &[u8]) -> Vec<u8> {
    let (w, h) = native_size(g);
    let mut img = vec![BLACK; w * h];
    let cell = g.cell;
    let (iw, _) = inner_dims(g);

    for r in 0..g.rows {
        for c in 0..g.cols {
            let is_border =
                r < g.border || r >= g.rows - g.border || c < g.border || c >= g.cols - g.border;
            let white = if is_border {
                border_expected(r, c)
            } else {
                let pr = r - g.border;
                let pc = c - g.border;
                let idx = pr * iw + pc;
                let byte_i = idx / 8;
                let bit = 7 - (idx % 8);
                byte_i < payload.len() && (payload[byte_i] >> bit) & 1 == 1
            };
            if white {
                let y0 = r * cell;
                let x0 = c * cell;
                let x1 = x0 + cell;
                for y in y0..y0 + cell {
                    let base = y * w;
                    img[base + x0..base + x1].fill(WHITE);
                }
            }
        }
    }
    img
}

fn median(buf: &mut [u8]) -> u8 {
    let n = buf.len();
    if n == 0 {
        return BLACK;
    }
    let (_, m, _) = buf.select_nth_unstable(n / 2);
    *m
}

pub struct Sampler {
    g: GridSpec,
    gw: usize,
    gh: usize,
    sx: f64,
    sy: f64,
    ox: i32,
    oy: i32,
}

impl Sampler {
    pub fn new(g: GridSpec, gw: usize, gh: usize, ox: i32, oy: i32) -> Self {
        let (nw, nh) = native_size(&g);
        Sampler {
            g,
            gw,
            gh,
            sx: gw as f64 / nw as f64,
            sy: gh as f64 / nh as f64,
            ox,
            oy,
        }
    }

    fn cell_median(&self, gray: &[u8], r: usize, c: usize) -> u8 {
        let cell = self.g.cell as f64;
        let x0 = (self.ox as f64 + c as f64 * cell * self.sx).floor() as i64;
        let x1 = (self.ox as f64 + (c + 1) as f64 * cell * self.sx).ceil() as i64;
        let y0 = (self.oy as f64 + r as f64 * cell * self.sy).floor() as i64;
        let y1 = (self.oy as f64 + (r + 1) as f64 * cell * self.sy).ceil() as i64;

        // Use only the interior of the cell: after a downscale (e.g. 1080p
        // -> 720p) the outer pixel ring of every cell is mixed with
        // neighbouring cells by the resampling filter, which drives the
        // median toward the neighbours. The interior stays clean.
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

        let mut buf = Vec::with_capacity(((x1 - x0 - 2 * px) * (y1 - y0 - 2 * py)).max(1) as usize);
        for y in y0 + py..y1 - py {
            let row = &gray[y as usize * self.gw..(y as usize + 1) * self.gw];
            buf.extend_from_slice(&row[(x0 + px) as usize..(x1 - px) as usize]);
        }
        median(&mut buf)
    }

    pub fn border_score(&self, gray: &[u8]) -> f64 {
        let g = &self.g;
        let mut ok = 0usize;
        let mut total = 0usize;
        for r in 0..g.rows {
            for c in 0..g.cols {
                let is_border = r < g.border
                    || r >= g.rows - g.border
                    || c < g.border
                    || c >= g.cols - g.border;
                if !is_border {
                    continue;
                }
                total += 1;
                let med = self.cell_median(gray, r, c);
                let white = med >= 128;
                if white == border_expected(r, c) {
                    ok += 1;
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            ok as f64 / total as f64
        }
    }

    pub fn payload(&self, gray: &[u8]) -> Vec<u8> {
        let g = &self.g;
        let nbytes = payload_bytes(g);
        let mut out = vec![0u8; nbytes];
        let mut bit_i = 0usize;
        for r in g.border..g.rows - g.border {
            for c in g.border..g.cols - g.border {
                let byte_i = bit_i / 8;
                if byte_i >= nbytes {
                    return out;
                }
                let med = self.cell_median(gray, r, c);
                if med >= 128 {
                    out[byte_i] |= 1 << (7 - (bit_i % 8));
                }
                bit_i += 1;
            }
        }
        out
    }
}

/// Search for pixel offset (ox, oy) that aligns the cell grid with the
/// decoded gray frame. Returns (ox, oy, border_match_fraction).
pub fn find_alignment(g: &GridSpec, gray: &[u8], gw: usize, gh: usize) -> (i32, i32, f64) {
    let (nw, _) = native_size(g);
    let cell_src = g.cell as f64 * (gw as f64 / nw as f64);
    let range_x = cell_src.ceil() as i32 + 1;
    let range_y = {
        let (_, nh) = native_size(g);
        let cell_src_y = g.cell as f64 * (gh as f64 / nh as f64);
        cell_src_y.ceil() as i32 + 1
    };
    let step = ((cell_src / 4.0).floor() as usize).max(1);

    let mut best = (0i32, 0i32, -1.0f64);
    let mut oy = -range_y;
    while oy <= range_y {
        let mut ox = -range_x;
        while ox <= range_x {
            let s = Sampler::new(*g, gw, gh, ox, oy).border_score(gray);
            if s > best.2 {
                best = (ox, oy, s);
            }
            ox += step as i32;
        }
        oy += step as i32;
    }
    best
}
