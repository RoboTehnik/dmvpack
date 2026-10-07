use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub fn spawn_encoder(
    output: &Path,
    width: usize,
    height: usize,
    fps: u32,
    crf: u32,
    preset: &str,
    audio_wav: Option<&Path>,
) -> Result<Child> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "gray".into(),
        "-s".into(),
        format!("{width}x{height}"),
        "-framerate".into(),
        fps.to_string(),
        "-i".into(),
        "pipe:0".into(),
    ];
    if let Some(wav) = audio_wav {
        args.push("-i".into());
        args.push(wav.display().to_string());
    }
    args.extend([
        "-c:v".to_string(),
        "libx264".to_string(),
        "-crf".to_string(),
        crf.to_string(),
        "-preset".to_string(),
        preset.to_string(),
        "-tune".to_string(),
        "stillimage".to_string(),
        "-g".to_string(),
        fps.to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
    ]);
    if audio_wav.is_some() {
        // no -shortest: the audio track may be longer than the video on
        // purpose (two full manifest copies must fit)
        args.extend([
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "96k".to_string(),
        ]);
    }
    args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    args.push(output.display().to_string());

    let child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn ffmpeg (is ffmpeg in PATH?)")?;
    Ok(child)
}

/// Decode the audio track of `path` to mono 48 kHz s16le samples.
pub fn extract_audio_pcm(path: &Path) -> Result<Vec<i16>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "48000", "-f", "s16le", "pipe:1"])
        .output()
        .context("failed to spawn ffmpeg (is ffmpeg in PATH?)")?;
    if !out.status.success() {
        bail!(
            "ffmpeg could not extract audio from {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if out.stdout.is_empty() {
        bail!(
            "no audio track in {} (the manifest is stored in audio)",
            path.display()
        );
    }
    let bytes = out.stdout;
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut pcm = Vec::with_capacity(pairs.len());
    for c in pairs {
        pcm.push(i16::from_le_bytes(*c));
    }
    Ok(pcm)
}

pub fn probe_size(path: &Path) -> Result<(usize, usize)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .context("failed to spawn ffprobe (is ffprobe in PATH?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let s = v["streams"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("no video stream in {}", path.display()))?;
    let w = s["width"].as_u64().ok_or_else(|| anyhow!("no width"))? as usize;
    let h = s["height"].as_u64().ok_or_else(|| anyhow!("no height"))? as usize;
    Ok((w, h))
}

pub struct FrameReader {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    frame_len: usize,
    consumed: usize,
}

impl FrameReader {
    pub fn open(path: &Path, width: usize, height: usize) -> Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-pix_fmt", "gray", "-f", "rawvideo", "pipe:1"])
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn ffmpeg (is ffmpeg in PATH?)")?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        Ok(FrameReader {
            child,
            stdout: BufReader::new(stdout),
            frame_len: width * height,
            consumed: 0,
        })
    }

    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; self.frame_len];
        let mut filled = 0usize;
        while filled < self.frame_len {
            let n = self.stdout.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Ok(None);
        }
        if filled < self.frame_len {
            bail!(
                "truncated frame #{} (got {filled}/{} bytes)",
                self.consumed,
                self.frame_len
            );
        }
        self.consumed += 1;
        Ok(Some(buf))
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Verify that `path` looks like a video and is readable.
pub fn check_video(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("video {} does not exist", path.display());
    }
    let _ = File::open(path)?;
    Ok(())
}
