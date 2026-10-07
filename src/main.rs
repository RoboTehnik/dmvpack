mod audio;
mod crypt;
mod ecc;
mod ffmpeg;
mod frame;
mod inner;
mod manifest;
mod pack;
mod unpack;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dmvpack",
    version,
    about = "Pack files into a black-and-white video that survives re-encoding",
    after_help = "Copyright 2026 Vladimir Sirenko <vmsirenko@gmail.com>"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Encode a file or directory into .mp4 (manifest embedded in audio)
    #[command(after_help = "Environment:
  DMVPACK_PASSWORD  password when -p is given without a value
  DMVPACK_DEBUG     trace manifest search and alignment
  DMVPACK_RS_DEBUG  inner Reed-Solomon statistics")]
    Pack {
        /// File or directory to archive
        input: PathBuf,
        /// Output video (default: <input>.mp4)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Split the output into parts of at most this size (e.g. 500M, 1G)
        #[arg(long)]
        max_part_size: Option<String>,
        /// Encrypt the archive (argon2id + chacha20-poly1305).
        /// `-p` without a value reads DMVPACK_PASSWORD
        #[arg(short, long, value_name = "PASSWORD", num_args = 0..=1, default_missing_value = "")]
        password: Option<String>,
        /// x264 preset (slower = smaller file)
        #[arg(long, default_value_t = "medium".to_string(), help_heading = "Advanced")]
        preset: String,
        /// x264 quality (lower = better quality, bigger file)
        #[arg(long, default_value_t = 16, help_heading = "Advanced")]
        crf: u32,
        /// Frames per second
        #[arg(long, default_value_t = 30, help_heading = "Advanced")]
        fps: u32,
        /// Cell size in pixels (>= 4)
        #[arg(long, default_value_t = 8, help_heading = "Advanced")]
        cell: usize,
        /// Grid columns
        #[arg(long, default_value_t = 240, help_heading = "Advanced")]
        cols: usize,
        /// Grid rows
        #[arg(long, default_value_t = 135, help_heading = "Advanced")]
        rows: usize,
        /// Frames per RS codeword group
        #[arg(long, default_value_t = 4, help_heading = "Advanced")]
        group: usize,
        /// Parity frames per group
        #[arg(long, default_value_t = 1, help_heading = "Advanced")]
        parity: usize,
        /// Also write the manifest as JSON to this path (debugging aid)
        #[arg(long, help_heading = "Advanced")]
        dump_manifest: Option<PathBuf>,
    },
    /// Decode .mp4 (manifest read from the audio track) back to the original;
    /// pass every part file if packed with --max-part-size
    #[command(after_help = "Environment:
  DMVPACK_PASSWORD  password if -p is not passed (tried automatically)
  DMVPACK_DEBUG     trace manifest search and alignment
  DMVPACK_RS_DEBUG  inner Reed-Solomon statistics")]
    Unpack {
        /// Video(s) produced by `pack`
        #[arg(required = true)]
        video: Vec<PathBuf>,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        outdir: PathBuf,
        /// Password for encrypted archives; `-p` without a value reads
        /// DMVPACK_PASSWORD (which is also tried automatically if not passed)
        #[arg(short, long, value_name = "PASSWORD", num_args = 0..=1, default_missing_value = "")]
        password: Option<String>,
        /// Read the manifest from this JSON file instead of the audio track
        #[arg(short, long, help_heading = "Advanced")]
        manifest: Option<PathBuf>,
    },
}

/// Resolve the pack password: absent = no encryption, empty = from env.
fn pack_password(flag: Option<String>) -> Result<Option<String>> {
    let p = match flag {
        None => return Ok(None),
        Some(p) if !p.is_empty() => p,
        Some(_) => std::env::var("DMVPACK_PASSWORD").unwrap_or_default(),
    };
    if p.is_empty() {
        anyhow::bail!("empty password: pass --password VALUE or set DMVPACK_PASSWORD");
    }
    Ok(Some(p))
}

/// Resolve the unpack password: flag first, then the env.
fn unpack_password(flag: Option<String>) -> Option<String> {
    match flag {
        Some(p) if !p.is_empty() => Some(p),
        _ => std::env::var("DMVPACK_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty()),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Pack {
            input,
            output,
            cell,
            cols,
            rows,
            fps,
            crf,
            group,
            parity,
            preset,
            dump_manifest,
            max_part_size,
            password,
        } => pack::run(pack::PackOpts {
            input,
            output,
            cell,
            cols,
            rows,
            fps,
            crf,
            group,
            parity,
            preset,
            dump_manifest,
            max_part_size,
            password: pack_password(password)?,
        }),
        Cmd::Unpack {
            video,
            manifest,
            outdir,
            password,
        } => unpack::run(unpack::UnpackOpts {
            videos: video,
            manifest,
            outdir,
            password: unpack_password(password),
        }),
    }
}
