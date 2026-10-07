mod audio;
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
    Pack {
        /// File or directory to archive
        input: PathBuf,
        /// Output video (default: <input>.mp4)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Cell size in pixels (>= 4)
        #[arg(long, default_value_t = 8)]
        cell: usize,
        /// Grid columns
        #[arg(long, default_value_t = 240)]
        cols: usize,
        /// Grid rows
        #[arg(long, default_value_t = 135)]
        rows: usize,
        /// Frames per second
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// x264 quality (lower = better)
        #[arg(long, default_value_t = 16)]
        crf: u32,
        /// Frames per RS codeword group
        #[arg(long, default_value_t = 4)]
        group: usize,
        /// Parity frames per group
        #[arg(long, default_value_t = 1)]
        parity: usize,
        /// x264 preset
        #[arg(long, default_value_t = "medium".to_string())]
        preset: String,
        /// Also write the manifest as JSON to this path (debugging aid)
        #[arg(long)]
        dump_manifest: Option<PathBuf>,
        /// Split the output into parts of at most this size (e.g. 500M, 1G)
        #[arg(long)]
        max_part_size: Option<String>,
    },
    /// Decode .mp4 (manifest read from the audio track) back to the original;
    /// pass every part file if packed with --max-part-size
    Unpack {
        /// Video(s) produced by `pack`
        #[arg(required = true)]
        video: Vec<PathBuf>,
        /// Read the manifest from this JSON file instead of the audio track
        #[arg(short, long)]
        manifest: Option<PathBuf>,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        outdir: PathBuf,
    },
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
        }),
        Cmd::Unpack {
            video,
            manifest,
            outdir,
        } => unpack::run(unpack::UnpackOpts {
            videos: video,
            manifest,
            outdir,
        }),
    }
}
