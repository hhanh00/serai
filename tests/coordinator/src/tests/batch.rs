use std::{
  sync::Mutex,
  time::Duration,
  collections::{HashSet, HashMap},
};

use zeroize::Zeroizing;
use rand_core::{RngCore, OsRng};

use ciphersuite::{group::GroupEncoding, Ciphersuite, Ristretto, Secp256k1};

use dkg::Participant;

use serai_client::{
  primitives::{NetworkId, BlockHash, Signature},
  in_instructions::{
    primitives::{Batch, SignedBatch, batch_message},
    InInstructionsEvent,
  },
};
use messages::{sign::SignId, SubstrateContext, CoordinatorMessage};

use crate::{*, tests::*};

pub async fn batch<C: Ciphersuite>(
  processors: &mut [Processor],
  substrate_key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
  network_key: &Zeroizing<C::F>,
  batch: Batch,
) -> u64 {
  let mut id = [0; 32];
  OsRng.fill_bytes(&mut id);
  let id = SignId { key: vec![], id, attempt: 0 };

  // Select a random participant to exclude, so we know for sure who *is* participating
  assert_eq!(COORDINATORS - THRESHOLD, 1);
  let excluded_signer =
    usize::try_from(OsRng.next_u64() % u64::try_from(processors.len()).unwrap()).unwrap();
  for (i, processor) in processors.iter_mut().enumerate() {
    if i == excluded_signer {
      continue;
    }

    processor
      .send_message(messages::coordinator::ProcessorMessage::BatchPreprocess {
        id: id.clone(),
        block: batch.block,
        preprocess: [u8::try_from(i).unwrap(); 64].to_vec(),
      })
      .await;
  }
  // Before this Batch is signed, the Tributary will agree this block occurred, adding an extra
  // step of latency
  wait_for_tributary().await;
  wait_for_tributary().await;

  // Send from the excluded signer so they don't stay stuck
  processors[excluded_signer]
    .send_message(messages::coordinator::ProcessorMessage::BatchPreprocess {
      id: id.clone(),
      block: batch.block,
      preprocess: [u8::try_from(excluded_signer).unwrap(); 64].to_vec(),
    })
    .await;

  // Read from a known signer to find out who was selected to sign
  let known_signer = (excluded_signer + 1) % COORDINATORS;
  let first_preprocesses = processors[known_signer].recv_message().await;
  let participants = match first_preprocesses {
    CoordinatorMessage::Coordinator(
      messages::coordinator::CoordinatorMessage::BatchPreprocesses { id: this_id, preprocesses },
    ) => {
      assert_eq!(&id, &this_id);
      assert_eq!(preprocesses.len(), THRESHOLD - 1);
      assert!(!preprocesses
        .contains_key(&Participant::new(u16::try_from(known_signer).unwrap() + 1).unwrap()));

      let mut participants =
        preprocesses.keys().map(|p| usize::from(u16::from(*p)) - 1).collect::<HashSet<_>>();
      for (p, preprocess) in preprocesses {
        assert_eq!(preprocess, vec![u8::try_from(u16::from(p)).unwrap() - 1; 64]);
      }
      participants.insert(known_signer);
      participants
    }
    _ => panic!("coordinator didn't send back BatchPreprocesses"),
  };

  for i in participants.clone() {
    if i == known_signer {
      continue;
    }
    let processor = &mut processors[i];
    let mut preprocesses = participants
      .clone()
      .into_iter()
      .map(|i| {
        (
          Participant::new(u16::try_from(i + 1).unwrap()).unwrap(),
          [u8::try_from(i).unwrap(); 64].to_vec(),
        )
      })
      .collect::<HashMap<_, _>>();
    preprocesses.remove(&Participant::new(u16::try_from(i + 1).unwrap()).unwrap());

    assert_eq!(
      processor.recv_message().await,
      CoordinatorMessage::Coordinator(
        messages::coordinator::CoordinatorMessage::BatchPreprocesses {
          id: id.clone(),
          preprocesses
        }
      )
    );
  }

  for i in participants.clone() {
    let processor = &mut processors[i];
    processor
      .send_message(messages::coordinator::ProcessorMessage::BatchShare {
        id: id.clone(),
        share: [u8::try_from(i).unwrap(); 32],
      })
      .await;
  }
  wait_for_tributary().await;
  for i in participants.clone() {
    let processor = &mut processors[i];
    let mut shares = participants
      .clone()
      .into_iter()
      .map(|i| {
        (Participant::new(u16::try_from(i + 1).unwrap()).unwrap(), [u8::try_from(i).unwrap(); 32])
      })
      .collect::<HashMap<_, _>>();
    shares.remove(&Participant::new(u16::try_from(i + 1).unwrap()).unwrap());

    assert_eq!(
      processor.recv_message().await,
      CoordinatorMessage::Coordinator(messages::coordinator::CoordinatorMessage::BatchShares {
        id: id.clone(),
        shares,
      })
    );
  }

  // Expand to a key pair as Schnorrkel expects
  // It's the private key + 32-bytes of entropy for nonces + the public key
  let mut schnorrkel_key_pair = [0; 96];
  schnorrkel_key_pair[.. 32].copy_from_slice(&substrate_key.to_repr());
  OsRng.fill_bytes(&mut schnorrkel_key_pair[32 .. 64]);
  schnorrkel_key_pair[64 ..]
    .copy_from_slice(&(<Ristretto as Ciphersuite>::generator() * **substrate_key).to_bytes());
  let signature = Signature(
    schnorrkel::keys::Keypair::from_bytes(&schnorrkel_key_pair)
      .unwrap()
      .sign_simple(b"substrate", &batch_message(&batch))
      .to_bytes(),
  );

  let batch = SignedBatch { batch, signature };

  let serai = processors[0].serai().await;
  let mut last_serai_block = serai.get_latest_block().await.unwrap().number();

  for processor in processors.iter_mut() {
    processor
      .send_message(messages::substrate::ProcessorMessage::Update { batch: batch.clone() })
      .await;
  }

  // Verify the Batch was published to Substrate
  'outer: for _ in 0 .. 20 {
    tokio::time::sleep(Duration::from_secs(6)).await;
    if std::env::var("GITHUB_CI") == Ok("true".to_string()) {
      tokio::time::sleep(Duration::from_secs(6)).await;
    }

    while last_serai_block <= serai.get_latest_block().await.unwrap().number() {
      let batch_events = serai
        .get_batch_events(
          serai.get_block_by_number(last_serai_block).await.unwrap().unwrap().hash(),
        )
        .await
        .unwrap();

      if !batch_events.is_empty() {
        assert_eq!(batch_events.len(), 1);
        assert_eq!(
          batch_events[0],
          InInstructionsEvent::Batch {
            network: batch.batch.network,
            id: batch.batch.id,
            block: batch.batch.block
          }
        );
        break 'outer;
      }
      last_serai_block += 1;
    }
  }

  // Verify the coordinator sends SubstrateBlock to all processors
  let last_block = serai.get_block_by_number(last_serai_block).await.unwrap().unwrap();
  for processor in processors.iter_mut() {
    assert_eq!(
      processor.recv_message().await,
      messages::CoordinatorMessage::Substrate(
        messages::substrate::CoordinatorMessage::SubstrateBlock {
          context: SubstrateContext {
            serai_time: last_block.time().unwrap() / 1000,
            network_latest_finalized_block: batch.batch.block,
          },
          network: batch.batch.network,
          block: last_serai_block,
          key: (C::generator() * **network_key).to_bytes().as_ref().to_vec(),
          burns: vec![],
          batches: vec![batch.batch.id],
        }
      )
    );

    // Send the ack as expected, though it shouldn't trigger any observable behavior
    processor
      .send_message(messages::ProcessorMessage::Coordinator(
        messages::coordinator::ProcessorMessage::SubstrateBlockAck {
          network: batch.batch.network,
          block: last_serai_block,
          plans: vec![],
        },
      ))
      .await;
  }
  last_block.number()
}

#[tokio::test]
async fn batch_test() {
  let _one_at_a_time = ONE_AT_A_TIME.get_or_init(|| Mutex::new(())).lock();
  let (processors, test) = new_test();

  test
    .run_async(|ops| async move {
      // Wait for the Serai node to boot, and for the Tendermint chain to get past the first block
      // TODO: Replace this with a Coordinator RPC
      tokio::time::sleep(Duration::from_secs(150)).await;

      // Sleep even longer if in the CI due to it being slower than commodity hardware
      if std::env::var("GITHUB_CI") == Ok("true".to_string()) {
        tokio::time::sleep(Duration::from_secs(120)).await;
      }

      // Connect to the Message Queues as the processor
      let mut new_processors: Vec<Processor> = vec![];
      for (handles, key) in processors {
        new_processors.push(Processor::new(NetworkId::Bitcoin, &ops, handles, key).await);
      }
      let mut processors = new_processors;

      let (substrate_key, network_key) = key_gen::<Secp256k1>(&mut processors).await;
      batch::<Secp256k1>(
        &mut processors,
        &substrate_key,
        &network_key,
        Batch {
          network: NetworkId::Bitcoin,
          id: 0,
          block: BlockHash([0x22; 32]),
          instructions: vec![],
        },
      )
      .await;
    })
    .await;
}
