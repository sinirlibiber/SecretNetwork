use log::*;

use sha2::Digest;

use crate::types::CosmosPubKey;

use enclave_crypto::traits::VerifyingKey;

use cosmos_proto::{crypto::multisig::multisig::MultiSignature, tx::signing::SignMode};

use super::traits::CosmosAminoPubkey;

use cw_types_v010::types::CanonicalAddr;
use enclave_crypto::CryptoError;
use protobuf::Message;

/// https://docs.tendermint.com/v0.32/spec/blockchain/encoding.html#public-key-cryptography
const MULTISIG_THRESHOLD_PREFIX: [u8; 4] = [0x22, 0xc1, 0xf7, 0xe2];
/// This is the result of
/// ```ignore
/// use prost::encoding::{encode_key, WireType};
/// let mut buf = vec![];
/// encode_key(1, WireType::Varint, &mut buf);
/// println!("{:?}", buf);
/// ```
const THRESHOLD_PREFIX: u8 = 0x08;
/// This is the result of (similar to above)
/// ```ignore
/// encode_key(2, WireType::LengthDelimited, &mut buf);
/// ```
const PUBKEY_PREFIX: u8 = 0x12;

#[derive(Debug, Clone, PartialEq)]
pub struct MultisigThresholdPubKey {
    threshold: u32,
    public_keys: Vec<CosmosPubKey>,
}

impl MultisigThresholdPubKey {
    pub fn new(threshold: u32, public_keys: Vec<CosmosPubKey>) -> Self {
        Self {
            threshold,
            public_keys,
        }
    }
}

impl CosmosAminoPubkey for MultisigThresholdPubKey {
    fn get_address(&self) -> CanonicalAddr {
        // Spec: https://docs.tendermint.com/master/spec/core/encoding.html#key-types
        // Multisig is undocumented, but we figured out it's the same as ed25519
        let address_bytes = &sha2::Sha256::digest(self.amino_bytes().as_slice())[..20];

        CanonicalAddr::from_vec(address_bytes.to_vec())
    }

    fn amino_bytes(&self) -> Vec<u8> {
        // Encoding for multisig is basically:
        // MULTISIG_THRESHOLD_PREFIX | THRESHOLD_PREFIX | threshold | [ PUBKEY_PREFIX | pubkey length | pubkey ] *
        let mut encoded = vec![];

        encoded.extend_from_slice(&MULTISIG_THRESHOLD_PREFIX);

        encoded.push(THRESHOLD_PREFIX);
        let mut threshold_bytes = vec![];
        prost::encoding::encode_varint(self.threshold as u64, &mut threshold_bytes);
        encoded.extend_from_slice(threshold_bytes.as_slice());

        for pubkey in &self.public_keys {
            encoded.push(PUBKEY_PREFIX);

            let pubkey_bytes = pubkey.amino_bytes();

            // Length may be more than 1 byte and it is protobuf encoded
            let mut length_bytes = vec![];
            // This line should never fail since it could only fail if `length`
            // does not have sufficient capacity to encode, but it's a vector
            // that gets extended
            if prost::encode_length_delimiter(pubkey_bytes.len(), &mut length_bytes).is_err() {
                error!(
                    "Could not encode length delimiter: {:?}. This should not happen",
                    pubkey_bytes.len()
                );
                return vec![];
            }
            encoded.extend_from_slice(&length_bytes);

            encoded.extend_from_slice(&pubkey_bytes);
        }

        trace!("pubkey bytes are: {:?}", encoded);
        encoded
    }
}

impl VerifyingKey for MultisigThresholdPubKey {
    fn verify_bytes(
        &self,
        bytes: &[u8],
        sig: &[u8],
        sign_mode: SignMode,
    ) -> Result<(), CryptoError> {
        debug!("verifying multisig");
        trace!("Sign bytes are: {:?}", bytes);
        let signatures = decode_multisig_signature(sig)?;

        if signatures.len() < self.threshold as usize {
            warn!(
                "insufficient signatures in multisig signature. found: {}, expected at least: {}",
                signatures.len(),
                self.public_keys.len()
            );
            return Err(CryptoError::VerificationError);
        }

        let mut verified_counter = 0;

        let mut signers: Vec<&CosmosPubKey> = self.public_keys.iter().collect();
        for current_sig in &signatures {
            trace!("Checking sig: {:?}", current_sig);
            if current_sig.is_empty() {
                trace!("skipping a signature because it was empty");
                continue;
            }

            let mut signer_pos = None;
            for (i, current_signer) in signers.iter().enumerate() {
                trace!("Checking pubkey: {:?}", current_signer);
                // This technically support that one of the multisig signers is a multisig itself
                let result = current_signer.verify_bytes(bytes, current_sig, sign_mode);

                if result.is_ok() {
                    signer_pos = Some(i);
                    verified_counter += 1;
                    break;
                }
            }

            // remove the signer that created this signature from the list to prevent a signer from signing multiple times
            if let Some(i) = signer_pos {
                signers.remove(i);
            } else {
                warn!(
                    "signature was not generated by any of the signers: {:?}",
                    current_sig
                );
                return Err(CryptoError::VerificationError);
            }
        }

        if verified_counter < self.threshold {
            warn!("Not enough valid signatures have been provided");
            Err(CryptoError::VerificationError)
        } else {
            debug!("Miltusig verified successfully");
            Ok(())
        }
    }
}

fn decode_multisig_signature(raw_blob: &[u8]) -> Result<Vec<Vec<u8>>, CryptoError> {
    let ms = MultiSignature::parse_from_bytes(raw_blob).map_err(|err| {
        warn!(
            "Failed to decode the signature of a multisig key from protobuf bytes: {:?}",
            err
        );
        CryptoError::ParsingError
    })?;

    Ok(ms.signatures.into_vec())
}

// TODO delete this function after verifying multisig works right
#[allow(unused)]
fn decode_multisig_signature_old(raw_blob: &[u8]) -> Result<Vec<Vec<u8>>, CryptoError> {
    trace!("decoding blob: {:?}", raw_blob);
    let blob_size = raw_blob.len();
    if blob_size < 8 {
        warn!("Multisig signature too short. decoding failed!");
        return Err(CryptoError::ParsingError);
    }

    let mut signatures: Vec<Vec<u8>> = vec![];

    let mut idx: usize = 7;
    while let Some(curr_blob_window) = raw_blob.get(idx..) {
        if curr_blob_window.is_empty() {
            break;
        }

        trace!("while letting with {:?}", curr_blob_window);
        trace!("blob len is {:?} idx is: {:?}", raw_blob.len(), idx);
        let current_sig_prefix = curr_blob_window[0];

        if current_sig_prefix != 0x12 {
            warn!("Multisig signature wrong prefix. decoding failed!");
            return Err(CryptoError::ParsingError);
        // The condition below can't fail because:
        // (1) curr_blob_window.get(1..) will return a Some(empty_slice) if curr_blob_window.len()=1
        // (2) At the beginning of the while loop we make sure curr_blob_window isn't empty, thus curr_blob_window.len() > 0
        // Therefore, no need for an else clause
        } else if let Some(sig_including_len) = curr_blob_window.get(1..) {
            // The condition below will take care of a case where `sig_including_len` is empty due
            // to curr_blob_window.get(), so no explicit check is needed here
            if let Ok(current_sig_len) = prost::decode_length_delimiter(sig_including_len) {
                let len_size = prost::length_delimiter_len(current_sig_len);

                trace!("sig len is: {:?}", current_sig_len);
                if let Some(raw_signature) =
                    sig_including_len.get(len_size..current_sig_len + len_size)
                {
                    signatures.push((&raw_signature).to_vec());
                    idx += 1 + len_size + current_sig_len; // prefix_byte + length_byte + len(sig)
                } else {
                    warn!("Multisig signature malformed. decoding failed!");
                    return Err(CryptoError::ParsingError);
                }
            } else {
                warn!("Multisig signature malformed. decoding failed!");
                return Err(CryptoError::ParsingError);
            }
        }
    }

    if signatures.is_empty() {
        warn!("Multisig signature empty. decoding failed!");
        return Err(CryptoError::ParsingError);
    }

    Ok(signatures)
}

#[cfg(feature = "test")]
pub mod tests_decode_multisig_signature {
    use super::decode_multisig_signature;

    pub fn test_decode_sig_sanity() {
        let expected = vec![vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10], vec![1, 2, 3, 4]];
        // let mut ms = MultiSignature::new();
        // ms.set_signatures(expected.into());
        // let sig = ms.write_to_bytes();
        // eprintln!("{:?}", sig);

        let sig = vec![10, 10, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 4, 1, 2, 3, 4];

        let result = decode_multisig_signature(sig.as_slice()).unwrap();
        assert_eq!(
            result, expected,
            "Signature is: {:?} and result is: {:?}",
            sig, result
        )
    }

    pub fn test_decode_long_leb128() {
        let expected = vec![vec![
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]];
        // let mut ms = MultiSignature::new();
        // ms.set_signatures(expected.into());
        // let sig = ms.write_to_bytes();
        // eprintln!("{:?}", sig);

        let sig = vec![
            10, 200, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0,
        ];

        let result = decode_multisig_signature(sig.as_slice()).unwrap();
        assert_eq!(
            result, expected,
            "Signature is: {:?} and result is: {:?}",
            sig, result
        )
    }

    pub fn test_decode_wrong_long_leb128() {
        let malformed_sig: Vec<u8> = vec![
            0, 0, 0, 0, 0, 0, 0, 0x12, 205, 1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
        ];

        let result = decode_multisig_signature(malformed_sig.as_slice());
        assert!(
            result.is_err(),
            "Signature is: {:?} and result is: {:?}",
            malformed_sig,
            result
        );
    }

    pub fn test_decode_malformed_sig_only_prefix() {
        let malformed_sig: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 0x12];

        let result = decode_multisig_signature(malformed_sig.as_slice());
        assert!(
            result.is_err(),
            "Signature is: {:?} and result is: {:?}",
            malformed_sig,
            result
        );
    }

    pub fn test_decode_sig_length_zero() {
        let expected: Vec<Vec<u8>> = vec![vec![]];
        // let mut ms = MultiSignature::new();
        // ms.set_signatures(expected.clone().into());
        // let sig = ms.write_to_bytes();
        // eprintln!("{:?}", sig);

        let sig = vec![10, 0];

        let result = decode_multisig_signature(sig.as_slice()).unwrap();
        assert_eq!(
            result, expected,
            "Signature is: {:?} and result is: {:?}",
            sig, result
        )
    }

    pub fn test_decode_malformed_sig_wrong_length() {
        let malformed_sig: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 0x12, 10, 0, 0];

        let result = decode_multisig_signature(malformed_sig.as_slice());
        assert!(
            result.is_err(),
            "Signature is: {:?} and result is: {:?}",
            malformed_sig,
            result
        );
    }
}
