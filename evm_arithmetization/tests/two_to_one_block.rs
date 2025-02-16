use env_logger::{try_init_from_env, Env, DEFAULT_FILTER_ENV};
use ethereum_types::{Address, BigEndianHash, H256};
use evm_arithmetization::fixed_recursive_verifier::{
    extract_block_public_values, extract_two_to_one_block_hash,
};
use evm_arithmetization::generation::{GenerationInputs, TrieInputs};
use evm_arithmetization::proof::{BlockMetadata, PublicValues, TrieRoots};
use evm_arithmetization::testing_utils::{
    beacon_roots_account_nibbles, beacon_roots_contract_from_storage, ger_account_nibbles,
    preinitialized_state_and_storage_tries, update_beacon_roots_account_storage,
    GLOBAL_EXIT_ROOT_ACCOUNT,
};
use evm_arithmetization::{AllRecursiveCircuits, AllStark, Node, StarkConfig};
use hex_literal::hex;
use mpt_trie::partial_trie::{HashedPartialTrie, PartialTrie};
use plonky2::field::goldilocks_field::GoldilocksField;
use plonky2::hash::poseidon::PoseidonHash;
use plonky2::plonk::config::{Hasher, PoseidonGoldilocksConfig};
use plonky2::plonk::proof::ProofWithPublicInputs;
use plonky2::util::timing::TimingTree;

type F = GoldilocksField;
const D: usize = 2;
type C = PoseidonGoldilocksConfig;

fn init_logger() {
    let _ = try_init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "info"));
}

/// Get `GenerationInputs` for a dummy payload, where the block has the given
/// timestamp.
fn dummy_payload(timestamp: u64, is_first_payload: bool) -> anyhow::Result<GenerationInputs> {
    let beneficiary = hex!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    let block_metadata = BlockMetadata {
        block_beneficiary: Address::from(beneficiary),
        block_timestamp: timestamp.into(),
        block_number: 1.into(),
        block_difficulty: 0x020000.into(),
        block_random: H256::from_uint(&0x020000.into()),
        block_gaslimit: 0xff112233u32.into(),
        block_chain_id: 1.into(),
        block_base_fee: 0xa.into(),
        ..Default::default()
    };

    let (mut state_trie_before, mut storage_tries) = preinitialized_state_and_storage_tries()?;
    let checkpoint_state_trie_root = state_trie_before.hash();
    let mut beacon_roots_account_storage = storage_tries[0].1.clone();

    update_beacon_roots_account_storage(
        &mut beacon_roots_account_storage,
        block_metadata.block_timestamp,
        block_metadata.parent_beacon_block_root,
    )?;
    let updated_beacon_roots_account =
        beacon_roots_contract_from_storage(&beacon_roots_account_storage);

    if !is_first_payload {
        // This isn't the first dummy payload being processed. We need to update the
        // initial state trie to account for the update on the beacon roots contract.
        state_trie_before.insert(
            beacon_roots_account_nibbles(),
            rlp::encode(&updated_beacon_roots_account).to_vec(),
        )?;
        storage_tries[0].1 = beacon_roots_account_storage;
    }

    let tries_before = TrieInputs {
        state_trie: state_trie_before,
        storage_tries,
        ..Default::default()
    };

    let expected_state_trie_after: HashedPartialTrie = {
        let mut state_trie_after = HashedPartialTrie::from(Node::Empty);
        state_trie_after.insert(
            beacon_roots_account_nibbles(),
            rlp::encode(&updated_beacon_roots_account).to_vec(),
        )?;
        state_trie_after.insert(
            ger_account_nibbles(),
            rlp::encode(&GLOBAL_EXIT_ROOT_ACCOUNT).to_vec(),
        )?;

        state_trie_after
    };

    let trie_roots_after = TrieRoots {
        state_root: expected_state_trie_after.hash(),
        transactions_root: tries_before.transactions_trie.hash(),
        receipts_root: tries_before.receipts_trie.hash(),
    };

    let inputs = GenerationInputs {
        tries: tries_before.clone(),
        trie_roots_after,
        checkpoint_state_trie_root,
        block_metadata,
        ..Default::default()
    };

    Ok(inputs)
}

fn get_test_block_proof(
    timestamp: u64,
    all_circuits: &AllRecursiveCircuits<GoldilocksField, PoseidonGoldilocksConfig, 2>,
    all_stark: &AllStark<GoldilocksField, 2>,
    config: &StarkConfig,
) -> anyhow::Result<ProofWithPublicInputs<GoldilocksField, PoseidonGoldilocksConfig, 2>> {
    let dummy0 = dummy_payload(timestamp, true)?;
    let dummy1 = dummy_payload(timestamp, false)?;

    let timing = &mut TimingTree::new(&format!("Blockproof {timestamp}"), log::Level::Info);
    let dummy0_proof0 =
        all_circuits.prove_all_segments(all_stark, config, dummy0, 20, timing, None)?;
    let dummy1_proof =
        all_circuits.prove_all_segments(all_stark, config, dummy1, 20, timing, None)?;

    let inputs0_proof = all_circuits.prove_segment_aggregation(
        false,
        &dummy0_proof0[0],
        false,
        &dummy0_proof0[1],
    )?;
    let dummy0_proof =
        all_circuits.prove_segment_aggregation(false, &dummy1_proof[0], false, &dummy1_proof[1])?;

    let (agg_proof0, pv0) = all_circuits.prove_transaction_aggregation(
        false,
        &inputs0_proof.proof_with_pis,
        inputs0_proof.public_values,
        false,
        &dummy0_proof.proof_with_pis,
        dummy0_proof.public_values,
    )?;

    all_circuits.verify_txn_aggregation(&agg_proof0)?;

    // Test retrieved public values from the proof public inputs.
    let retrieved_public_values0 = PublicValues::from_public_inputs(&agg_proof0.public_inputs);
    assert_eq!(retrieved_public_values0, pv0);
    assert_eq!(
        pv0.trie_roots_before.state_root,
        pv0.extra_block_data.checkpoint_state_trie_root
    );

    let (block_proof0, block_public_values) = all_circuits.prove_block(
        None, // We don't specify a previous proof, considering block 1 as the new checkpoint.
        &agg_proof0,
        pv0.clone(),
    )?;

    let pv_block = PublicValues::from_public_inputs(&block_proof0.public_inputs);
    assert_eq!(block_public_values, pv_block.into());

    Ok(block_proof0)
}

#[ignore]
#[test]
fn test_two_to_one_block_aggregation() -> anyhow::Result<()> {
    init_logger();
    let some_timestamps = [127, 42, 65, 43];

    let all_stark = AllStark::<F, D>::default();
    let config = StarkConfig::standard_fast_config();
    let all_circuits = AllRecursiveCircuits::<F, C, D>::new(
        &all_stark,
        &[
            16..17,
            9..15,
            12..18,
            14..15,
            9..10,
            12..13,
            17..20,
            16..17,
            7..8,
        ],
        &config,
    );

    let unrelated_block_proofs = some_timestamps
        .iter()
        .map(|&ts| get_test_block_proof(ts, &all_circuits, &all_stark, &config))
        .collect::<anyhow::Result<Vec<ProofWithPublicInputs<F, C, D>>>>()?;

    unrelated_block_proofs
        .iter()
        .try_for_each(|bp| all_circuits.verify_block(bp))?;

    let bp = unrelated_block_proofs;

    {
        // Aggregate the same proof twice
        let aggproof_42_42 = all_circuits.prove_two_to_one_block(&bp[0], false, &bp[0], false)?;
        all_circuits.verify_two_to_one_block(&aggproof_42_42)?;
    }

    {
        // Binary tree reduction
        //
        //  A    B    C    D    Blockproofs (base case)
        //   \  /      \  /
        //  (A, B)    (C, D)    Two-to-one block aggregation proofs
        //     \       /
        //   ((A,B), (C,D))     Two-to-one block aggregation proofs

        let aggproof01 = all_circuits.prove_two_to_one_block(&bp[0], false, &bp[1], false)?;
        all_circuits.verify_two_to_one_block(&aggproof01)?;

        let aggproof23 = all_circuits.prove_two_to_one_block(&bp[2], false, &bp[3], false)?;
        all_circuits.verify_two_to_one_block(&aggproof23)?;

        let aggproof0123 =
            all_circuits.prove_two_to_one_block(&aggproof01, true, &aggproof23, true)?;
        all_circuits.verify_two_to_one_block(&aggproof0123)?;

        {
            // Compute Merkle root from public inputs of block proofs.
            // Leaves
            let mut hashes: Vec<_> = bp
                .iter()
                .map(|block_proof| {
                    let public_values = extract_block_public_values(&block_proof.public_inputs);
                    PoseidonHash::hash_no_pad(public_values)
                })
                .collect();

            // Inner nodes
            hashes.extend_from_within(0..hashes.len());
            let half = hashes.len() / 2;
            for i in 0..half - 1 {
                hashes[half + i] = PoseidonHash::two_to_one(hashes[2 * i], hashes[2 * i + 1]);
            }
            let merkle_root = hashes[hashes.len() - 2].elements;

            assert_eq!(
                extract_two_to_one_block_hash(&aggproof0123.public_inputs),
                &merkle_root,
                "Merkle root of verifier's verification tree did not match merkle root in public inputs."
            );
        }
    }

    {
        // Foldleft
        //
        //  A    B    C    D    Blockproofs (base case)
        //   \  /    /    /
        //  (A, B)  /    /      Two-to-one block aggregation proofs
        //     \   /    /
        //  ((A,B), C) /        Two-to-one block aggregation proofs
        //       \    /
        //  (((A,B),C),D)       Two-to-one block aggregation proofs

        let aggproof01 = all_circuits.prove_two_to_one_block(&bp[0], false, &bp[1], false)?;
        all_circuits.verify_two_to_one_block(&aggproof01)?;

        let aggproof012 = all_circuits.prove_two_to_one_block(&aggproof01, true, &bp[2], false)?;
        all_circuits.verify_two_to_one_block(&aggproof012)?;

        let aggproof0123 =
            all_circuits.prove_two_to_one_block(&aggproof012, true, &bp[3], false)?;
        all_circuits.verify_two_to_one_block(&aggproof0123)?;
    }

    {
        // Foldright
        //
        //  A    B    C    D    Blockproofs (base case)
        //   \    \   \   /
        //    \    \   (C,D)    Two-to-one block aggregation proofs
        //     \     \  /
        //      \ (B,(C, D))    Two-to-one block aggregation proofs
        //       \   /
        //     (A,(B,(C,D)))    Two-to-one block aggregation proofs

        let aggproof23 = all_circuits.prove_two_to_one_block(&bp[2], false, &bp[3], false)?;
        all_circuits.verify_two_to_one_block(&aggproof23)?;

        let aggproof123 = all_circuits.prove_two_to_one_block(&bp[1], false, &aggproof23, true)?;
        all_circuits.verify_two_to_one_block(&aggproof123)?;

        let aggproof0123 =
            all_circuits.prove_two_to_one_block(&bp[0], false, &aggproof123, true)?;
        all_circuits.verify_two_to_one_block(&aggproof0123)?;
    }

    Ok(())
}
