use std::io;
use std::io::{Cursor, Read, Write};
use byteorder::{LE, ReadBytesExt, WriteBytesExt};
use std_shims::{sync::OnceLock, vec::Vec, collections::HashSet};

#[cfg(not(feature = "std"))]
use std_shims::sync::Mutex;
#[cfg(feature = "std")]
use futures::lock::Mutex;

use zeroize::{Zeroize, ZeroizeOnDrop};

use rand_core::{RngCore, CryptoRng};
use rand_distr::{Distribution, Gamma};
#[cfg(not(feature = "std"))]
use rand_distr::num_traits::Float;

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};

use crate::{
  serialize::varint_len,
  wallet::SpendableOutput,
  rpc::{RpcError, RpcConnection, Rpc},
};
use crate::serialize::write_varint;

const LOCK_WINDOW: usize = 10;
const MATURITY: u64 = 60;
const RECENT_WINDOW: usize = 15;
const BLOCK_TIME: usize = 120;
const BLOCKS_PER_YEAR: usize = 365 * 24 * 60 * 60 / BLOCK_TIME;
const TIP_APPLICATION: f64 = (LOCK_WINDOW * BLOCK_TIME) as f64;

// TODO: Expose an API to reset this in case a reorg occurs/the RPC fails/returns garbage
// TODO: Update this when scanning a block, as possible
static DISTRIBUTION_CELL: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
#[allow(non_snake_case)]
fn DISTRIBUTION() -> &'static Mutex<Vec<u64>> {
  DISTRIBUTION_CELL.get_or_init(|| Mutex::new(Vec::with_capacity(3000000)))
}

#[allow(clippy::too_many_arguments)]
async fn select_n<'a, R: RngCore + CryptoRng, RPC: RpcConnection>(
  rng: &mut R,
  rpc: &Rpc<RPC>,
  distribution: &[u64],
  height: usize,
  high: u64,
  per_second: f64,
  real: &[u64],
  used: &mut HashSet<u64>,
  count: usize,
) -> Result<Vec<(u64, [EdwardsPoint; 2])>, RpcError> {
  if height >= rpc.get_height().await? {
    // TODO: Don't use InternalError for the caller's failure
    Err(RpcError::InternalError("decoys being requested from too young blocks"))?;
  }

  #[cfg(test)]
  let mut iters = 0;
  let mut confirmed = Vec::with_capacity(count);
  // Retries on failure. Retries are obvious as decoys, yet should be minimal
  while confirmed.len() != count {
    let remaining = count - confirmed.len();
    let mut candidates = Vec::with_capacity(remaining);
    while candidates.len() != remaining {
      #[cfg(test)]
      {
        iters += 1;
        // This is cheap and on fresh chains, a lot of rounds may be needed
        if iters == 100 {
          Err(RpcError::InternalError("hit decoy selection round limit"))?;
        }
      }

      // Use a gamma distribution
      let mut age = Gamma::<f64>::new(19.28, 1.0 / 1.61).unwrap().sample(rng).exp();
      if age > TIP_APPLICATION {
        age -= TIP_APPLICATION;
      } else {
        // f64 does not have try_from available, which is why these are written with `as`
        age = (rng.next_u64() % u64::try_from(RECENT_WINDOW * BLOCK_TIME).unwrap()) as f64;
      }

      let o = (age * per_second) as u64;
      if o < high {
        let i = distribution.partition_point(|s| *s < (high - 1 - o));
        let prev = i.saturating_sub(1);
        let n = distribution[i] - distribution[prev];
        if n != 0 {
          let o = distribution[prev] + (rng.next_u64() % n);
          if !used.contains(&o) {
            // It will either actually be used, or is unusable and this prevents trying it again
            used.insert(o);
            candidates.push(o);
          }
        }
      }
    }

    // If this is the first time we're requesting these outputs, include the real one as well
    // Prevents the node we're connected to from having a list of known decoys and then seeing a
    // TX which uses all of them, with one additional output (the true spend)
    let mut real_indexes = HashSet::with_capacity(real.len());
    if confirmed.is_empty() {
      for real in real {
        candidates.push(*real);
      }
      // Sort candidates so the real spends aren't the ones at the end
      candidates.sort();
      for real in real {
        real_indexes.insert(candidates.binary_search(real).unwrap());
      }
    }

    for (i, output) in rpc.get_unlocked_outputs(&candidates, height).await?.iter_mut().enumerate() {
      // Don't include the real spend as a decoy, despite requesting it
      if real_indexes.contains(&i) {
        continue;
      }

      if let Some(output) = output.take() {
        confirmed.push((candidates[i], output));
      }
    }
  }

  Ok(confirmed)
}

fn offset(ring: &[u64]) -> Vec<u64> {
  let mut res = vec![ring[0]];
  res.resize(ring.len(), 0);
  for m in (1 .. ring.len()).rev() {
    res[m] = ring[m] - ring[m - 1];
  }
  res
}

/// Decoy data, containing the actual member as well (at index `i`).
#[derive(Clone, PartialEq, Eq, Debug, Zeroize, ZeroizeOnDrop)]
pub struct Decoys {
  pub(crate) i: u8,
  pub(crate) offsets: Vec<u64>,
  pub(crate) ring: Vec<[EdwardsPoint; 2]>,
}

#[allow(clippy::len_without_is_empty)]
impl Decoys {
  pub fn fee_weight(offsets: &[u64]) -> usize {
    varint_len(offsets.len()) + offsets.iter().map(|offset| varint_len(*offset)).sum::<usize>()
  }

  pub fn len(&self) -> usize {
    self.offsets.len()
  }

  /// Select decoys using the same distribution as Monero.
  pub async fn select<R: RngCore + CryptoRng, RPC: RpcConnection>(
    rng: &mut R,
    rpc: &Rpc<RPC>,
    ring_len: usize,
    height: usize,
    inputs: &[SpendableOutput],
  ) -> Result<Vec<Decoys>, RpcError> {
    #[cfg(not(feature = "std"))]
    let mut distribution = DISTRIBUTION().lock();
    #[cfg(feature = "std")]
    let mut distribution = DISTRIBUTION().lock().await;

    let decoy_count = ring_len - 1;

    // Convert the inputs in question to the raw output data
    let mut real = Vec::with_capacity(inputs.len());
    let mut outputs = Vec::with_capacity(inputs.len());
    for input in inputs {
      real.push(input.global_index);
      outputs.push((real[real.len() - 1], [input.key(), input.commitment().calculate()]));
    }

    if distribution.len() <= height {
      let extension = rpc.get_output_distribution(distribution.len(), height).await?;
      distribution.extend(extension);
    }
    // If asked to use an older height than previously asked, truncate to ensure accuracy
    // Should never happen, yet risks desyncing if it did
    distribution.truncate(height + 1); // height is inclusive, and 0 is a valid height

    let high = distribution[distribution.len() - 1];
    let per_second = {
      let blocks = distribution.len().min(BLOCKS_PER_YEAR);
      let outputs = high - distribution[distribution.len().saturating_sub(blocks + 1)];
      (outputs as f64) / ((blocks * BLOCK_TIME) as f64)
    };

    let mut used = HashSet::<u64>::new();
    for o in &outputs {
      used.insert(o.0);
    }

    // TODO: Create a TX with less than the target amount, as allowed by the protocol
    if (high - MATURITY) < u64::try_from(inputs.len() * ring_len).unwrap() {
      Err(RpcError::InternalError("not enough decoy candidates"))?;
    }

    // Select all decoys for this transaction, assuming we generate a sane transaction
    // We should almost never naturally generate an insane transaction, hence why this doesn't
    // bother with an overage
    let mut decoys = select_n(
      rng,
      rpc,
      &distribution,
      height,
      high,
      per_second,
      &real,
      &mut used,
      inputs.len() * decoy_count,
    )
    .await?;
    real.zeroize();

    let mut res = Vec::with_capacity(inputs.len());
    for o in outputs {
      // Grab the decoys for this specific output
      let mut ring = decoys.drain((decoys.len() - decoy_count) ..).collect::<Vec<_>>();
      ring.push(o);
      ring.sort_by(|a, b| a.0.cmp(&b.0));

      // Sanity checks are only run when 1000 outputs are available in Monero
      // We run this check whenever the highest output index, which we acknowledge, is > 500
      // This means we assume (for presumably test blockchains) the height being used has not had
      // 500 outputs since while itself not being a sufficiently mature blockchain
      // Considering Monero's p2p layer doesn't actually check transaction sanity, it should be
      // fine for us to not have perfectly matching rules, especially since this code will infinite
      // loop if it can't determine sanity, which is possible with sufficient inputs on
      // sufficiently small chains
      if high > 500 {
        // Make sure the TX passes the sanity check that the median output is within the last 40%
        let target_median = high * 3 / 5;
        while ring[ring_len / 2].0 < target_median {
          // If it's not, update the bottom half with new values to ensure the median only moves up
          for removed in ring.drain(0 .. (ring_len / 2)).collect::<Vec<_>>() {
            // If we removed the real spend, add it back
            if removed.0 == o.0 {
              ring.push(o);
            } else {
              // We could not remove this, saving CPU time and removing low values as
              // possibilities, yet it'd increase the amount of decoys required to create this
              // transaction and some removed outputs may be the best option (as we drop the first
              // half, not just the bottom n)
              used.remove(&removed.0);
            }
          }

          // Select new outputs until we have a full sized ring again
          ring.extend(
            select_n(
              rng,
              rpc,
              &distribution,
              height,
              high,
              per_second,
              &[],
              &mut used,
              ring_len - ring.len(),
            )
            .await?,
          );
          ring.sort_by(|a, b| a.0.cmp(&b.0));
        }

        // The other sanity check rule is about duplicates, yet we already enforce unique ring
        // members
      }

      res.push(Decoys {
        // Binary searches for the real spend since we don't know where it sorted to
        i: u8::try_from(ring.partition_point(|x| x.0 < o.0)).unwrap(),
        offsets: offset(&ring.iter().map(|output| output.0).collect::<Vec<_>>()),
        ring: ring.iter().map(|output| output.1).collect(),
      });
    }

    Ok(res)
  }

  pub fn read<R: Read>(r: &mut R) -> io::Result<Decoys> {
    let i = r.read_u8()?;
    let n = r.read_u32::<LE>()? as usize;
    let mut offsets = vec![];
    let mut ring = vec![];
    for _ in 0..n {
      offsets.push(r.read_u64::<LE>()?);
    }
    for _ in 0..n {
      let mut p = [0u8; 32];
      r.read_exact(&mut p)?;
      let p1 = CompressedEdwardsY::from_slice(&p).unwrap().decompress().unwrap();
      r.read_exact(&mut p)?;
      let p2 = CompressedEdwardsY::from_slice(&p).unwrap().decompress().unwrap();
      ring.push([p1, p2]);
    }
    Ok(Decoys {
      i,
      offsets,
      ring,
    })
  }

  pub fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
    w.write_u8(self.i)?;
    w.write_u32::<LE>(self.offsets.len() as u32)?;
    for &o in self.offsets.iter() {
      w.write_u64::<LE>(o)?;
    }
    for &o in self.ring.iter() {
      w.write_all(o[0].compress().as_bytes())?;
      w.write_all(o[1].compress().as_bytes())?;
    }
    Ok(())
  }

  pub fn from_bytes(buf: &[u8]) -> Self {
    let mut c = Cursor::new(buf);
    Self::read(&mut c).unwrap()
  }

  pub fn to_bytes(&self) -> Vec<u8> {
    let mut buf = vec![];
    self.write(&mut buf).unwrap();
    buf
  }
}
