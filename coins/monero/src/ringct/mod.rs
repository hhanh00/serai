use core::ops::Deref;
use std_shims::{
  vec::Vec,
  io::{self, Read, Write},
};

use zeroize::{Zeroize, Zeroizing};

use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, scalar::Scalar, edwards::EdwardsPoint};

pub(crate) mod hash_to_point;
pub use hash_to_point::{raw_hash_to_point, hash_to_point};

/// MLSAG struct, along with verifying functionality.
pub mod mlsag;
/// CLSAG struct, along with signing and verifying functionality.
pub mod clsag;
/// BorromeanRange struct, along with verifying functionality.
pub mod borromean;
/// Bulletproofs(+) structs, along with proving and verifying functionality.
pub mod bulletproofs;

use crate::{
  Protocol,
  serialize::*,
  ringct::{mlsag::Mlsag, clsag::Clsag, borromean::BorromeanRange, bulletproofs::Bulletproofs},
};

/// Generate a key image for a given key. Defined as `x * hash_to_point(xG)`.
pub fn generate_key_image(secret: &Zeroizing<Scalar>) -> EdwardsPoint {
  hash_to_point(ED25519_BASEPOINT_TABLE * secret.deref()) * secret.deref()
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EncryptedAmount {
  Original { mask: [u8; 32], amount: [u8; 32] },
  Compact { amount: [u8; 8] },
}

impl EncryptedAmount {
  pub fn read<R: Read>(compact: bool, r: &mut R) -> io::Result<EncryptedAmount> {
    Ok(if !compact {
      EncryptedAmount::Original { mask: read_bytes(r)?, amount: read_bytes(r)? }
    } else {
      EncryptedAmount::Compact { amount: read_bytes(r)? }
    })
  }

  pub fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
    match self {
      EncryptedAmount::Original { mask, amount } => {
        w.write_all(mask)?;
        w.write_all(amount)
      }
      EncryptedAmount::Compact { amount } => w.write_all(amount),
    }
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Zeroize)]
pub enum RctType {
  /// No RCT proofs.
  Null,
  /// One MLSAG for a single input and a Borromean range proof (RCTTypeFull).
  MlsagAggregate,
  // One MLSAG for each input and a Borromean range proof (RCTTypeSimple).
  MlsagIndividual,
  // One MLSAG for each input and a Bulletproof (RCTTypeBulletproof).
  Bulletproofs,
  /// One MLSAG for each input and a Bulletproof, yet starting to use EncryptedAmount::Compact
  /// (RCTTypeBulletproof2).
  BulletproofsCompactAmount,
  /// One CLSAG for each input and a Bulletproof (RCTTypeCLSAG).
  Clsag,
  /// One CLSAG for each input and a Bulletproof+ (RCTTypeBulletproofPlus).
  BulletproofsPlus,
}

impl RctType {
  pub fn to_byte(self) -> u8 {
    match self {
      RctType::Null => 0,
      RctType::MlsagAggregate => 1,
      RctType::MlsagIndividual => 2,
      RctType::Bulletproofs => 3,
      RctType::BulletproofsCompactAmount => 4,
      RctType::Clsag => 5,
      RctType::BulletproofsPlus => 6,
    }
  }

  pub fn from_byte(byte: u8) -> Option<Self> {
    Some(match byte {
      0 => RctType::Null,
      1 => RctType::MlsagAggregate,
      2 => RctType::MlsagIndividual,
      3 => RctType::Bulletproofs,
      4 => RctType::BulletproofsCompactAmount,
      5 => RctType::Clsag,
      6 => RctType::BulletproofsPlus,
      _ => None?,
    })
  }

  pub fn compact_encrypted_amounts(&self) -> bool {
    match self {
      RctType::Null => false,
      RctType::MlsagAggregate => false,
      RctType::MlsagIndividual => false,
      RctType::Bulletproofs => false,
      RctType::BulletproofsCompactAmount => true,
      RctType::Clsag => true,
      RctType::BulletproofsPlus => true,
    }
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RctBase {
  pub fee: u64,
  pub pseudo_outs: Vec<EdwardsPoint>,
  pub encrypted_amounts: Vec<EncryptedAmount>,
  pub commitments: Vec<EdwardsPoint>,
}

impl RctBase {
  pub(crate) fn fee_weight(outputs: usize, fee: u64) -> usize {
    // 1 byte for the RCT signature type
    1 + (outputs * (8 + 32)) + varint_len(fee)
  }

  pub fn write<W: Write>(&self, w: &mut W, rct_type: RctType) -> io::Result<()> {
    w.write_all(&[rct_type.to_byte()])?;
    match rct_type {
      RctType::Null => Ok(()),
      _ => {
        write_varint(&self.fee, w)?;
        if rct_type == RctType::MlsagIndividual {
          write_raw_vec(write_point, &self.pseudo_outs, w)?;
        }
        for encrypted_amount in &self.encrypted_amounts {
          encrypted_amount.write(w)?;
        }
        write_raw_vec(write_point, &self.commitments, w)
      }
    }
  }

  pub fn read<R: Read>(inputs: usize, outputs: usize, r: &mut R) -> io::Result<(RctBase, RctType)> {
    let rct_type = RctType::from_byte(read_byte(r)?)
      .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "invalid RCT type"))?;

    match rct_type {
      RctType::Null => {}
      RctType::MlsagAggregate => {}
      RctType::MlsagIndividual => {}
      RctType::Bulletproofs |
      RctType::BulletproofsCompactAmount |
      RctType::Clsag |
      RctType::BulletproofsPlus => {
        if outputs == 0 {
          // Because the Bulletproofs(+) layout must be canonical, there must be 1 Bulletproof if
          // Bulletproofs are in use
          // If there are Bulletproofs, there must be a matching amount of outputs, implicitly
          // banning 0 outputs
          // Since HF 12 (CLSAG being 13), a 2-output minimum has also been enforced
          Err(io::Error::new(io::ErrorKind::Other, "RCT with Bulletproofs(+) had 0 outputs"))?;
        }
      }
    }

    Ok((
      if rct_type == RctType::Null {
        RctBase { fee: 0, pseudo_outs: vec![], encrypted_amounts: vec![], commitments: vec![] }
      } else {
        RctBase {
          fee: read_varint(r)?,
          pseudo_outs: if rct_type == RctType::MlsagIndividual {
            read_raw_vec(read_point, inputs, r)?
          } else {
            vec![]
          },
          encrypted_amounts: (0 .. outputs)
            .map(|_| EncryptedAmount::read(rct_type.compact_encrypted_amounts(), r))
            .collect::<Result<_, _>>()?,
          commitments: read_raw_vec(read_point, outputs, r)?,
        }
      },
      rct_type,
    ))
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RctPrunable {
  Null,
  MlsagBorromean {
    borromean: Vec<BorromeanRange>,
    mlsags: Vec<Mlsag>,
  },
  MlsagBulletproofs {
    bulletproofs: Bulletproofs,
    mlsags: Vec<Mlsag>,
    pseudo_outs: Vec<EdwardsPoint>,
  },
  Clsag {
    bulletproofs: Bulletproofs,
    clsags: Vec<Clsag>,
    pseudo_outs: Vec<EdwardsPoint>,
  },
}

impl RctPrunable {
  pub(crate) fn fee_weight(protocol: Protocol, inputs: usize, outputs: usize) -> usize {
    // 1 byte for number of BPs (technically a VarInt, yet there's always just zero or one)
    1 + Bulletproofs::fee_weight(protocol.bp_plus(), outputs) +
      (inputs * (Clsag::fee_weight(protocol.ring_len()) + 32))
  }

  pub fn write<W: Write>(&self, w: &mut W, rct_type: RctType) -> io::Result<()> {
    match self {
      RctPrunable::Null => Ok(()),
      RctPrunable::MlsagBorromean { borromean, mlsags } => {
        write_raw_vec(BorromeanRange::write, borromean, w)?;
        write_raw_vec(Mlsag::write, mlsags, w)
      }
      RctPrunable::MlsagBulletproofs { bulletproofs, mlsags, pseudo_outs } => {
        if rct_type == RctType::Bulletproofs {
          w.write_all(&1u32.to_le_bytes())?;
        } else {
          w.write_all(&[1])?;
        }
        bulletproofs.write(w)?;

        write_raw_vec(Mlsag::write, mlsags, w)?;
        write_raw_vec(write_point, pseudo_outs, w)
      }
      RctPrunable::Clsag { bulletproofs, clsags, pseudo_outs } => {
        w.write_all(&[1])?;
        bulletproofs.write(w)?;

        write_raw_vec(Clsag::write, clsags, w)?;
        write_raw_vec(write_point, pseudo_outs, w)
      }
    }
  }

  pub fn serialize(&self, rct_type: RctType) -> Vec<u8> {
    let mut serialized = vec![];
    self.write(&mut serialized, rct_type).unwrap();
    serialized
  }

  pub fn read<R: Read>(
    rct_type: RctType,
    decoys: &[usize],
    outputs: usize,
    r: &mut R,
  ) -> io::Result<RctPrunable> {
    // While we generally don't bother with misc consensus checks, this affects the safety of
    // the below defined rct_type function
    // The exact line preventing zero-input transactions is:
    // https://github.com/monero-project/monero/blob/00fd416a99686f0956361d1cd0337fe56e58d4a7/
    //   src/ringct/rctSigs.cpp#L609
    // And then for RctNull, that's only allowed for miner TXs which require one input of
    // Input::Gen
    if decoys.is_empty() {
      Err(io::Error::new(io::ErrorKind::Other, "transaction had no inputs"))?;
    }

    Ok(match rct_type {
      RctType::Null => RctPrunable::Null,
      RctType::MlsagAggregate | RctType::MlsagIndividual => RctPrunable::MlsagBorromean {
        borromean: read_raw_vec(BorromeanRange::read, outputs, r)?,
        mlsags: decoys.iter().map(|d| Mlsag::read(*d, r)).collect::<Result<_, _>>()?,
      },
      RctType::Bulletproofs | RctType::BulletproofsCompactAmount => {
        RctPrunable::MlsagBulletproofs {
          bulletproofs: {
            if (if rct_type == RctType::Bulletproofs {
              u64::from(read_u32(r)?)
            } else {
              read_varint(r)?
            }) != 1
            {
              Err(io::Error::new(io::ErrorKind::Other, "n bulletproofs instead of one"))?;
            }
            Bulletproofs::read(r)?
          },
          mlsags: decoys.iter().map(|d| Mlsag::read(*d, r)).collect::<Result<_, _>>()?,
          pseudo_outs: read_raw_vec(read_point, decoys.len(), r)?,
        }
      }
      RctType::Clsag | RctType::BulletproofsPlus => RctPrunable::Clsag {
        bulletproofs: {
          if read_varint(r)? != 1 {
            Err(io::Error::new(io::ErrorKind::Other, "n bulletproofs instead of one"))?;
          }
          (if rct_type == RctType::Clsag { Bulletproofs::read } else { Bulletproofs::read_plus })(
            r,
          )?
        },
        clsags: (0 .. decoys.len()).map(|o| Clsag::read(decoys[o], r)).collect::<Result<_, _>>()?,
        pseudo_outs: read_raw_vec(read_point, decoys.len(), r)?,
      },
    })
  }

  pub(crate) fn signature_write<W: Write>(&self, w: &mut W) -> io::Result<()> {
    match self {
      RctPrunable::Null => panic!("Serializing RctPrunable::Null for a signature"),
      RctPrunable::MlsagBorromean { borromean, .. } => {
        borromean.iter().try_for_each(|rs| rs.write(w))
      }
      RctPrunable::MlsagBulletproofs { bulletproofs, .. } => bulletproofs.signature_write(w),
      RctPrunable::Clsag { bulletproofs, .. } => bulletproofs.signature_write(w),
    }
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RctSignatures {
  pub base: RctBase,
  pub prunable: RctPrunable,
}

impl RctSignatures {
  /// RctType for a given RctSignatures struct.
  pub fn rct_type(&self) -> RctType {
    match &self.prunable {
      RctPrunable::Null => RctType::Null,
      RctPrunable::MlsagBorromean { .. } => {
        /*
          This type of RctPrunable may have no outputs, yet pseudo_outs are per input
          This will only be a valid RctSignatures if it's for a TX with inputs
          That makes this valid for any valid RctSignatures

          While it will be invalid for any invalid RctSignatures, potentially letting an invalid
          MlsagAggregate be interpreted as a valid MlsagIndividual (or vice versa), they have
          incompatible deserializations

          This means it's impossible to receive a MlsagAggregate over the wire and interpret it
          as a MlsagIndividual (or vice versa)

          That only makes manual manipulation unsafe, which will always be true since these fields
          are all pub

          TODO: Consider making them private with read-only accessors?
        */
        if self.base.pseudo_outs.is_empty() {
          RctType::MlsagAggregate
        } else {
          RctType::MlsagIndividual
        }
      }
      // RctBase ensures there's at least one output, making the following
      // inferences guaranteed/expects impossible on any valid RctSignatures
      RctPrunable::MlsagBulletproofs { .. } => {
        if matches!(
          self
            .base
            .encrypted_amounts
            .get(0)
            .expect("MLSAG with Bulletproofs didn't have any outputs"),
          EncryptedAmount::Original { .. }
        ) {
          RctType::Bulletproofs
        } else {
          RctType::BulletproofsCompactAmount
        }
      }
      RctPrunable::Clsag { bulletproofs, .. } => {
        if matches!(bulletproofs, Bulletproofs::Original { .. }) {
          RctType::Clsag
        } else {
          RctType::BulletproofsPlus
        }
      }
    }
  }

  pub(crate) fn fee_weight(protocol: Protocol, inputs: usize, outputs: usize, fee: u64) -> usize {
    RctBase::fee_weight(outputs, fee) + RctPrunable::fee_weight(protocol, inputs, outputs)
  }

  pub fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
    let rct_type = self.rct_type();
    self.base.write(w, rct_type)?;
    self.prunable.write(w, rct_type)
  }

  pub fn serialize(&self) -> Vec<u8> {
    let mut serialized = vec![];
    self.write(&mut serialized).unwrap();
    serialized
  }

  pub fn read<R: Read>(decoys: Vec<usize>, outputs: usize, r: &mut R) -> io::Result<RctSignatures> {
    let base = RctBase::read(decoys.len(), outputs, r)?;
    Ok(RctSignatures { base: base.0, prunable: RctPrunable::read(base.1, &decoys, outputs, r)? })
  }
}
