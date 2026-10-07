use anyhow::{bail, Result};
use reed_solomon_erasure::galois_8::Field;
use reed_solomon_erasure::ReedSolomon;

/// Encode a group of shards in place: shards[0..data_frames] hold data
/// (zero-padded by the caller), the rest become parity.
pub fn encode_group(shards: &mut [Vec<u8>], data_frames: usize) -> Result<()> {
    let n = shards.len();
    if data_frames >= n {
        bail!("invalid RS parameters: data_frames={data_frames} n={n}");
    }
    let rs: ReedSolomon<Field> = ReedSolomon::new(data_frames, n - data_frames)?;
    rs.encode(shards)
        .map_err(|e| anyhow::anyhow!("rs encode: {e}"))?;
    Ok(())
}

/// Reconstruct missing shards (flagged false) in place.
/// Each element is (shard data, present flag).
pub fn repair_group(shards: &mut [(Vec<u8>, bool)], data_frames: usize) -> Result<()> {
    let n = shards.len();
    let missing = shards.iter().filter(|s| !s.1).count();
    if missing == 0 {
        return Ok(());
    }
    if missing > n - data_frames {
        bail!(
            "{missing} damaged frames in group, only {} recoverable",
            n - data_frames
        );
    }
    let rs: ReedSolomon<Field> = ReedSolomon::new(data_frames, n - data_frames)?;
    rs.reconstruct(shards)
        .map_err(|e| anyhow::anyhow!("rs reconstruct: {e}"))?;
    Ok(())
}
