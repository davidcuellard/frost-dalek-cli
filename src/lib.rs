use frost_dalek::signature::SecretKey as SignatureSecretKey;
use frost_dalek::signature::ThresholdSignature;
use frost_dalek::{
    compute_message_hash, generate_commitment_share_lists, DistributedKeyGeneration, GroupKey,
    Parameters, Participant, SignatureAggregator,
};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::from_reader;
use std::fs::File;
use std::io::BufReader;

#[derive(Serialize, Deserialize)]
pub struct FrostKeys {
    pub group_key: [u8; 32],
    pub private_shares: Vec<([u8; 32], u32)>,
    pub threshold: u32,
}

/// Generates a public key and private key shares using FROST.
///
/// # Parameters
/// - `t`: Threshold value, the minimum number of participants required to reconstruct the private key.
/// - `n`: Total number of participants (key shares).
///
/// # Returns
/// - Saves the keys to `./results/frost_keys.json` in JSON format.
///
/// Generates the public key and private key shares.
pub fn generate_keys(
    t: u32,
    n: u32,
    output_key_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // check if the threshold is less than the total number of participants
    if t > n {
        return Err(
            "Threshold value cannot be greater than the total number of participants".into(),
        );
    }

    // Initialize the parameters for the key generation.
    let params = Parameters { t, n };

    // Step 1: Create participants and their polynomial coefficients.
    let mut participants = Vec::new();
    let mut coefficients = Vec::new();
    for i in 1..=n {
        let (participant, coeff) = Participant::new(&params, i);
        participants.push(participant);
        coefficients.push(coeff);
    }

    // Step 2: Verify zero-knowledge proof of secret keys for all participants.
    for participant in &participants {
        participant
            .proof_of_secret_key
            .verify(&participant.index, &participant.public_key().unwrap())
            .map_err(|_| {
                format!(
                    "Proof of secret key verification failed for participant {}",
                    participant.index
                )
            })?;
    }
    println!("All participants verified their proofs of secret keys!");

    // Step 3: Perform the first round of Distributed Key Generation (DKG).
    let mut dkg_states = Vec::new();
    let mut all_secret_shares = Vec::new();
    for (i, participant) in participants.iter().enumerate() {
        let mut other_participants = participants.clone();
        other_participants.remove(i);

        let participant_state = DistributedKeyGeneration::<_>::new(
            &params,
            &participant.index,
            &coefficients[i],
            &mut other_participants,
        )
        .map_err(|err| {
            format!(
                "DistributedKeyGeneration failed for participant: {}: {:?}",
                &participant.index, err
            )
        })?;

        let participant_their_secret_shares = participant_state
            .their_secret_shares()
            .map_err(|_| {
                format!(
                    "Secret shares retrieval failed for participant {}",
                    participant.index
                )
            })?
            .to_vec();

        dkg_states.push(participant_state);
        all_secret_shares.push(participant_their_secret_shares);
    }
    println!("DKG Round 1 complete");

    // Step 4: Share secret shares and complete Round 2 of DKG.
    let mut dkg_states_round_two = Vec::new();
    for (i, dkg_state) in dkg_states.into_iter().enumerate() {
        let my_secret_shares: Vec<_> = all_secret_shares
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .filter_map(|(j, shares)| {
                let pos = if i < j { i } else { i - 1 };
                shares.get(pos).cloned()
            })
            .collect();

        // Ensure the correct number of shares are received.
        if my_secret_shares.len() != (params.n - 1) as usize {
            return Err(format!(
                "Participant {} received incorrect number of shares: expected {}, got {}",
                participants[i].index,
                params.n - 1,
                my_secret_shares.len()
            )
            .into());
        }

        let round_two_state = dkg_state
            .to_round_two(my_secret_shares)
            .map_err(|_| format!("Round 2 failed for participant {}", participants[i].index))?;

        dkg_states_round_two.push(round_two_state);
    }
    println!("Share secret shares Round 2 complete");

    // Step 5: Finalize DKG and save the keys.
    let mut group_keys = Vec::new();
    let mut private_shares = Vec::new();
    for (i, dkg_state) in dkg_states_round_two.iter().enumerate() {
        let (dkg_group_key, dkg_secret_key) = dkg_state
            .clone()
            .finish(participants[i].public_key().unwrap())
            .map_err(|_| {
                format!(
                    "Failed to finish DKG for participant {}",
                    participants[i].index
                )
            })?;

        group_keys.push(dkg_group_key);
        private_shares.push(dkg_secret_key.to_bytes());

        // Ensure all group keys are identical.
        if i > 0 {
            assert_eq!(dkg_group_key, group_keys[i - 1]);
        }
    }

    // Combine group key and private shares into a single structure.
    let frost_keys = FrostKeys {
        group_key: group_keys[0].to_bytes(),
        private_shares,
        threshold: t,
    };

    // Save the keys to a JSON file.
    let file = File::create(output_key_file)?;
    serde_json::to_writer_pretty(file, &frost_keys)?;

    println!("Generated {} shares with threshold {}. Keys saved.", n, t);
    Ok(())
}

/// Signs a message using threshold signing.
///
/// # Arguments
/// - `message`: The message to be signed.
/// - `t`: The signing threshold (minimum participants required).
/// - `n`: The total number of participants.
/// - `key_file`: Path to the file containing the generated keys.
/// - `signature_file`: Path to save the generated signature.
///
/// # Errors
/// Returns an error if loading keys, generating commitment shares, or signing fails.
pub fn sign_message(
    message: &str,
    signers: Vec<u32>,
    n: u32,
    key_file: &str,
    signature_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Load keys from file
    let file = File::open(key_file)?;
    let reader = BufReader::new(file);
    let frost_keys: FrostKeys = from_reader(reader)?;

    // Step 2: Check if the number of participants matches the key file
    if frost_keys.private_shares.len() != n as usize {
        return Err("Number of participants does not match the key file".into());
    }

    // Step 3: Check if the number of signers is at least the threshold
    if signers.len() < frost_keys.threshold as usize {
        return Err("Number of signers is less than the threshold".into());
    }

    // Step 4: Ensure all specified signers are valid
    for &signer in &signers {
        if signer as usize >= frost_keys.private_shares.len() {
            return Err(format!("Invalid signer index: {}", signer).into());
        }
    }

    // Step 5: Load the group public key
    let group_key =
        GroupKey::from_bytes(frost_keys.group_key).map_err(|_| "Invalid group public key")?;

    // Step 6: Reconstruct secret keys for the specified signers
    let mut secret_keys = Vec::new();
    for &signer in &signers {
        let (key_bytes, index) = frost_keys.private_shares[signer as usize];
        let secret_key = SignatureSecretKey::from_bytes(index, key_bytes)
            .map_err(|_| "Invalid private key bytes")?;
        secret_keys.push(secret_key);
    }

    // Step 7: Generate commitment shares for the chosen signers
    let mut public_comshares = Vec::new();
    let mut secret_comshares = Vec::new();
    for signer in &secret_keys {
        let (pub_com, sec_com) = generate_commitment_share_lists(&mut OsRng, signer.get_index(), 1);
        public_comshares.push((signer.get_index(), pub_com));
        secret_comshares.push((signer.get_index(), sec_com));
    }

    // Step 8: Hash the message to create a signing context
    let context = b"THRESHOLD SIGNING CONTEXT";
    let message_bytes = message.as_bytes();
    let message_hash = compute_message_hash(&context[..], &message_bytes[..]);

    // Step 9: Initialize a signature aggregator
    let mut aggregator = SignatureAggregator::new(
        Parameters {
            t: frost_keys.threshold,
            n,
        },
        group_key,
        &context[..],
        &message_bytes[..],
    );

    // Step 10: Include signers and their commitment shares in the aggregator
    for (signer, (index, pub_com)) in secret_keys.iter().zip(public_comshares.iter()) {
        let public_key = signer.to_public();
        aggregator.include_signer(*index, pub_com.commitments[0], public_key);
    }

    // Step 11: Get the list of participating signers
    let signers = aggregator.get_signers().clone();

    // Step 12: Create and include partial signatures
    for (secret_key, (_, sec_com)) in secret_keys.iter().zip(secret_comshares.iter_mut()) {
        let partial_sig = secret_key.sign(&message_hash, &group_key, sec_com, 0, &signers)?;
        aggregator.include_partial_signature(partial_sig);
    }

    // Step 13: Finalize and aggregate the threshold signature
    let aggregator = aggregator.finalize().map_err(|err| {
        let error_message = format!("Failed to finalize aggregator: {:?}", err);
        Box::<dyn std::error::Error>::from(error_message)
    })?;

    let threshold_signature = aggregator.aggregate().map_err(|err| {
        let error_message = format!("Failed to aggregate signature: {:?}", err);
        Box::<dyn std::error::Error>::from(error_message)
    })?;

    // Step 14: Save the signature as a JSON file
    let file = File::create(signature_file)?;
    serde_json::to_writer_pretty(file, &threshold_signature.to_bytes().to_vec())?;

    println!("Threshold signature saved to: {}", signature_file);
    Ok(())
}

/// Validates a threshold signature for a given message.
///
/// This function ensures that a provided signature matches the expected
/// group public key and is valid for the provided message.
///
/// # Arguments
///
/// - `message`: The message whose signature needs validation.
/// - `key_file`: Path to the JSON file containing the group public key and private shares.
/// - `signature_file`: Path to the JSON file containing the threshold signature.
///
/// # Returns
///
/// - `Ok(())` if the signature is valid.
/// - An error if the signature is invalid or if any validation step fails.
pub fn validate_signature(
    message: &str,
    key_file: &str,
    signature_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Load the signature from file
    let signature_file = File::open(signature_file)?;
    let signature_reader = BufReader::new(signature_file);

    // Parse the signature into a vector of bytes
    let signature_vec: Vec<u8> = serde_json::from_reader(signature_reader)?;
    if signature_vec.len() != 64 {
        return Err("Invalid length for threshold signature".into());
    }

    // Convert signature bytes to a fixed-length array
    let signature_bytes: [u8; 64] = signature_vec
        .try_into()
        .map_err(|_| "Failed to convert to [u8; 64]")?;

    // Deserialize the threshold signature
    let threshold_signature = ThresholdSignature::from_bytes(signature_bytes)
        .map_err(|_| "Failed to deserialize ThresholdSignature")?;

    // Step 2: Load the public group key from the key file
    let key_file = File::open(key_file)?;
    let key_reader = BufReader::new(key_file);
    let frost_keys: FrostKeys = serde_json::from_reader(key_reader)?;

    // Reconstruct the group public key
    let group_key =
        GroupKey::from_bytes(frost_keys.group_key).map_err(|_| "Invalid group public key")?;

    // Step 3: Compute the message hash
    let context = b"THRESHOLD SIGNING CONTEXT";
    let message_bytes = message.as_bytes();
    let message_hash = compute_message_hash(&context[..], &message_bytes[..]);

    // Step 4: Verify the threshold signature
    threshold_signature
        .verify(&group_key, &message_hash)
        .map_err(|_| "Signature verification failed")?;

    println!("Signature is valid!");
    Ok(())
}
