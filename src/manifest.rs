use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GridSpec {
    pub cols: usize,
    pub rows: usize,
    pub cell: usize,
    pub border: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EccSpec {
    pub group_frames: usize,
    pub parity_frames: usize,
    pub shard_bytes: usize,
}

impl EccSpec {
    pub fn data_frames(&self) -> usize {
        self.group_frames - self.parity_frames
    }
    pub fn data_per_group(&self) -> usize {
        self.data_frames() * self.shard_bytes
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoSpec {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub crf: u32,
    pub frames: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PayloadSpec {
    pub raw_len: u64,
    pub raw_sha256: String,
    pub compressed_len: u64,
    pub groups: usize,
    pub source_name: String,
    pub source_is_dir: bool,
}

/// Slice of the payload carried by one part of a split archive.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartSpec {
    /// Zero-based part index.
    pub index: u16,
    /// Byte offset of this slice inside the compressed payload.
    pub payload_offset: u64,
    /// Length of this slice.
    pub payload_len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub grid: GridSpec,
    pub ecc: EccSpec,
    pub video: VideoSpec,
    pub payload: PayloadSpec,
    pub frame_crc: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<PartSpec>,
}

const MAGIC: &[u8; 4] = b"VARC";
const FORMAT_VERSION: u16 = 1;

impl Manifest {
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)
            .with_context(|| format!("cannot write manifest {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read manifest {}", path.display()))?;
        let m: Manifest = serde_json::from_str(&data)
            .with_context(|| format!("invalid manifest {}", path.display()))?;
        if m.version != 1 {
            anyhow::bail!("unsupported manifest version {}", m.version);
        }
        Ok(m)
    }

    /// Compact binary encoding embedded in the audio track.
    ///
    /// Layout (little-endian):
    /// magic, version, grid, ecc, video, payload (with length-prefixed
    /// strings), frame_crc (u16 per frame), crc32 of everything before it.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b: Vec<u8> = Vec::with_capacity(128 + self.frame_crc.len() * 2);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        b.extend_from_slice(&(self.grid.cols as u16).to_le_bytes());
        b.extend_from_slice(&(self.grid.rows as u16).to_le_bytes());
        b.extend_from_slice(&(self.grid.cell as u16).to_le_bytes());
        b.extend_from_slice(&(self.grid.border as u16).to_le_bytes());
        b.extend_from_slice(&(self.ecc.group_frames as u16).to_le_bytes());
        b.extend_from_slice(&(self.ecc.parity_frames as u16).to_le_bytes());
        b.extend_from_slice(&(self.ecc.shard_bytes as u32).to_le_bytes());
        b.extend_from_slice(&(self.video.width as u32).to_le_bytes());
        b.extend_from_slice(&(self.video.height as u32).to_le_bytes());
        b.extend_from_slice(&(self.video.fps as u16).to_le_bytes());
        b.extend_from_slice(&(self.video.crf as u16).to_le_bytes());
        b.extend_from_slice(&(self.video.frames as u32).to_le_bytes());
        b.extend_from_slice(&self.payload.raw_len.to_le_bytes());
        b.extend_from_slice(&self.payload.compressed_len.to_le_bytes());
        b.extend_from_slice(&(self.payload.groups as u32).to_le_bytes());
        b.push(self.payload.source_is_dir as u8);
        push_str(&mut b, &self.payload.raw_sha256);
        push_str(&mut b, &self.payload.source_name);
        for &c in &self.frame_crc {
            b.extend_from_slice(&c.to_le_bytes());
        }
        let crc = crc32fast::hash(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        if let Some(p) = &self.part {
            let mut t = Vec::with_capacity(24);
            t.extend_from_slice(b"VPT1");
            t.extend_from_slice(&p.index.to_le_bytes());
            t.extend_from_slice(&0u16.to_le_bytes());
            t.extend_from_slice(&p.payload_offset.to_le_bytes());
            t.extend_from_slice(&p.payload_len.to_le_bytes());
            let tcrc = crc32fast::hash(&t);
            b.extend_from_slice(&t);
            b.extend_from_slice(&tcrc.to_le_bytes());
        }
        b
    }

    pub fn from_bytes(data: &[u8]) -> Result<Manifest> {
        let mut r = Reader { d: data, p: 0 };
        if r.take(4)? != MAGIC {
            bail!("manifest magic mismatch");
        }
        let ver = r.u16()?;
        if ver != FORMAT_VERSION {
            bail!("unsupported manifest format version {ver}");
        }
        let grid = GridSpec {
            cols: r.u16()? as usize,
            rows: r.u16()? as usize,
            cell: r.u16()? as usize,
            border: r.u16()? as usize,
        };
        let ecc = EccSpec {
            group_frames: r.u16()? as usize,
            parity_frames: r.u16()? as usize,
            shard_bytes: r.u32()? as usize,
        };
        let video = VideoSpec {
            width: r.u32()? as usize,
            height: r.u32()? as usize,
            fps: r.u16()? as u32,
            crf: r.u16()? as u32,
            frames: r.u32()? as usize,
        };
        let payload = PayloadSpec {
            raw_len: r.u64()?,
            compressed_len: r.u64()?,
            groups: r.u32()? as usize,
            source_is_dir: r.u8()? != 0,
            raw_sha256: r.string()?.to_string(),
            source_name: r.string()?.to_string(),
        };

        if grid.cols == 0 || grid.rows == 0 || grid.cell < 4 {
            bail!("corrupt manifest: bad grid");
        }
        if ecc.group_frames < 2 || ecc.parity_frames >= ecc.group_frames || ecc.shard_bytes == 0 {
            bail!("corrupt manifest: bad ecc spec");
        }
        if video.frames == 0 || payload.groups == 0 {
            bail!("corrupt manifest: empty video/payload");
        }

        let need = video.frames * 2;
        let avail = data.len().saturating_sub(r.p);
        if avail < need + 4 {
            bail!(
                "truncated manifest: need {} bytes of crcs, have {}",
                need,
                avail.saturating_sub(4)
            );
        }
        let mut frame_crc = Vec::with_capacity(video.frames);
        for _ in 0..video.frames {
            frame_crc.push(r.u16()?);
        }
        let got = r.u32()?;
        let want = crc32fast::hash(&data[..r.p - 4]);
        if got != want {
            bail!("manifest crc32 mismatch (got {got:08x}, want {want:08x})");
        }
        // optional split-part trailer after the main crc (ignored by old readers)
        let mut part = None;
        let rest = &data[r.p..];
        if rest.len() >= 28 && &rest[..4] == b"VPT1" {
            let tcrc = u32::from_le_bytes(rest[24..28].try_into().unwrap());
            if crc32fast::hash(&rest[..24]) != tcrc {
                bail!("manifest part trailer crc32 mismatch");
            }
            part = Some(PartSpec {
                index: u16::from_le_bytes(rest[4..6].try_into().unwrap()),
                payload_offset: u64::from_le_bytes(rest[8..16].try_into().unwrap()),
                payload_len: u64::from_le_bytes(rest[16..24].try_into().unwrap()),
            });
        }
        Ok(Manifest {
            version: 1,
            grid,
            ecc,
            video,
            payload,
            frame_crc,
            part,
        })
    }
}

fn push_str(b: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    b.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    b.extend_from_slice(bytes);
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.p + n > self.d.len() {
            bail!("manifest truncated at byte {}", self.p);
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<&'a str> {
        let n = self.u16()? as usize;
        std::str::from_utf8(self.take(n)?).map_err(|_| anyhow!("invalid utf8 in manifest"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
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
            part: None,
        }
    }

    #[test]
    fn bytes_roundtrip() {
        let m = sample();
        let b = m.to_bytes();
        let back = Manifest::from_bytes(&b).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn detects_corruption() {
        let m = sample();
        let mut b = m.to_bytes();
        let n = b.len();
        b[n / 2] ^= 0xff;
        assert!(Manifest::from_bytes(&b).is_err());
    }

    #[test]
    fn tolerates_trailing_garbage() {
        let m = sample();
        let mut b = m.to_bytes();
        b.extend_from_slice(&[0x13, 0x37, 0x00, 0xff]);
        let back = Manifest::from_bytes(&b).unwrap();
        assert_eq!(m, back);
        assert!(back.part.is_none());
    }

    #[test]
    fn part_trailer_roundtrip() {
        let mut m = sample();
        m.part = Some(PartSpec {
            index: 3,
            payload_offset: 123_456,
            payload_len: 7890,
        });
        let b = m.to_bytes();
        let back = Manifest::from_bytes(&b).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.part.unwrap().index, 3);
    }

    #[test]
    fn detects_part_trailer_corruption() {
        let mut m = sample();
        m.part = Some(PartSpec {
            index: 1,
            payload_offset: 10,
            payload_len: 20,
        });
        let mut b = m.to_bytes();
        let n = b.len();
        b[n - 5] ^= 0xff; // inside the trailer body
        assert!(Manifest::from_bytes(&b).is_err());
    }
}
