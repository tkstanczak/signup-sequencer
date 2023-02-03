#![allow(unused_variables, dead_code)] // TODO [AA] Remove when this is used outside of tests.
mod identity;
mod proof;

use crate::prover::{identity::Identity, proof::Proof};
use clap::Parser;
use ethers::{types::U256, utils::keccak256};
use reqwest;
use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Formatter},
    mem::size_of,
    time::Duration,
};
use url::Url;

/// The endpoint used for proving operations.
const MTB_PROVE_ENDPOINT: &str = "prove";

/// Configuration options for the component responsible for interacting with the
/// prover service.
#[derive(Clone, Debug, PartialEq, Eq, Parser)]
#[group(skip)]
pub struct Options {
    /// The URL at which to contact the semaphore prover service for proof
    /// generation.
    #[clap(long, env, default_value = "http://localhost:3001")]
    pub mtb_prover_url: String,

    /// The number of seconds to wait before timing out the transaction.
    #[clap(long, env, default_value = "30")]
    pub mtb_prover_timeout_secs: u64,

    // TODO Add and query a prover `info` endpoint instead.
    /// The batch size that the prover is set up to work with. This must match
    /// the deployed prover.
    #[clap(long, env, default_value = "50")]
    pub batch_size: usize,
}

/// A representation of the connection to the MTB prover service.
#[derive(Clone, Debug)]
pub struct Prover {
    target_url: Url,
    client:     reqwest::Client,
    batch_size: usize,
}

impl Prover {
    /// Constructs a new instance of the Merkle Tree Batcher (or Mtb).
    ///
    /// # Arguments
    /// - `options`: The prover configuration options.
    pub fn new(options: &Options) -> anyhow::Result<Self> {
        let target_url = Url::parse(&options.mtb_prover_url)?;
        let timeout_duration = Duration::from_secs(options.mtb_prover_timeout_secs);
        let batch_size = options.batch_size;
        let client = reqwest::Client::builder()
            .connect_timeout(timeout_duration)
            .https_only(false)
            .build()?;
        let mtb = Self {
            target_url,
            client,
            batch_size,
        };

        Ok(mtb)
    }

    /// Generates a proof term for the provided identity insertions into the
    /// merkle tree.
    ///
    /// # Arguments
    /// - `start_index`: The index in the merkle tree at which the insertions
    ///   were started.
    /// - `pre_root`: The value of the merkle tree's root before identities were
    ///   inserted.
    /// - `post_root`: The value of the merkle tree's root after the identities
    ///   were inserted.
    /// - `identities`: A list of identity insertions, ordered in the order the
    ///   identities were inserted into the merkle tree.
    pub async fn generate_proof(
        &self,
        start_index: u32,
        pre_root: U256,
        post_root: U256,
        identities: Vec<Identity>,
    ) -> anyhow::Result<Proof> {
        if identities.len() != self.batch_size {
            return Err(anyhow::Error::msg(
                "Provided batch does not match prover batch size.",
            ));
        }

        let identity_commitments: Vec<U256> = identities.iter().map(|id| id.commitment).collect();
        let input_hash =
            compute_input_hash(start_index, pre_root, post_root, &identity_commitments);
        let merkle_proofs = identities
            .iter()
            .map(|id| id.merkle_proof.clone())
            .collect();

        let proof_input = ProofInput {
            input_hash,
            start_index,
            pre_root,
            post_root,
            identity_commitments,
            merkle_proofs,
        };

        let request = self
            .client
            .post(self.target_url.join(MTB_PROVE_ENDPOINT)?)
            .body("OH MY GOD")
            .json(&proof_input)
            .build()?;
        let proof_term = self.client.execute(request).await?;
        let json = proof_term.text().await?;

        let Ok(proof) = serde_json::from_str::<Proof>(&json) else {
            let error: ProverError = serde_json::from_str(&json)?;
            return Err(anyhow::Error::msg(format!("{error}")))
        };

        Ok(proof)
    }
}

/// Computes the input hash to the prover.
///
/// The input hash is specified as the `keccak256` hash of the inputs arranged
/// as follows:
///
/// ```md
/// StartIndex || PreRoot || PostRoot || IdComms[0] || IdComms[1] || ... || IdComms[batchSize-1]
///     32     ||   256   ||   256    ||    256     ||    256     || ... ||      256 bits
/// ```
///
/// where:
/// - `StartIndex` is `start_index`, the leaf index in the tree from which the
///   insertions started.
/// - `PreRoot` is `pre_root`, the root value of the merkle tree before the
///   insertions were made.
/// - `PostRoot` is `post_root`, the root value of the merkle tree after the
///   insertions were made.
/// - `IdComms` is `identity_commitments`, the list of identity commitments
///   provided in the order that they were inserted into the tree.
///
/// The result is computed using the inputs in _big-endian_ byte ordering.
pub fn compute_input_hash(
    start_index: u32,
    pre_root: U256,
    post_root: U256,
    identity_commitments: &[U256],
) -> U256 {
    let mut pre_root_bytes: [u8; size_of::<U256>()] = Default::default();
    pre_root.to_big_endian(pre_root_bytes.as_mut_slice());
    let mut post_root_bytes: [u8; size_of::<U256>()] = Default::default();
    post_root.to_big_endian(post_root_bytes.as_mut_slice());

    let mut bytes: Vec<u8> = vec![];
    bytes.extend_from_slice(&start_index.to_be_bytes());
    bytes.extend(pre_root_bytes.iter());
    bytes.extend(post_root_bytes.iter());

    for commitment in identity_commitments.iter() {
        let mut commitment_bytes: [u8; size_of::<U256>()] = Default::default();
        commitment.to_big_endian(commitment_bytes.as_mut_slice());
        bytes.extend(commitment_bytes.iter());
    }

    keccak256(bytes).into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProverError {
    pub code:    String,
    pub message: String,
}

impl Display for ProverError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PROVER FAILURE: Code = {}, Message = {}",
            self.code, self.message
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProofInput {
    input_hash:           U256,
    start_index:          u32,
    pre_root:             U256,
    post_root:            U256,
    identity_commitments: Vec<U256>,
    merkle_proofs:        Vec<Vec<U256>>,
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn mtb_should_generate_proof_with_correct_inputs() -> anyhow::Result<()> {
        let mock_url: String = "0.0.0.0:3001".into();
        let mock_service = mock::Service::new(mock_url.clone()).await?;

        let options = Options {
            mtb_prover_url:          "http://localhost:3001".into(),
            mtb_prover_timeout_secs: 30,
            batch_size:              3,
        };
        let mtb = Prover::new(&options).unwrap();
        let input_data = get_default_proof_input();
        let identities: Vec<Identity> = extract_identities_from(&input_data);

        let expected_proof = get_default_proof_output();
        let proof = mtb
            .generate_proof(
                input_data.start_index,
                input_data.pre_root,
                input_data.post_root,
                identities,
            )
            .await?;

        mock_service.stop();

        assert_eq!(proof, expected_proof);

        Ok(())
    }

    #[tokio::test]
    async fn mtb_should_respond_with_error_if_inputs_incorrect() -> anyhow::Result<()> {
        let mock_url: String = "0.0.0.0:3002".into();
        let mock_service = mock::Service::new(mock_url.clone()).await?;

        let options = Options {
            mtb_prover_url:          "http://localhost:3002".into(),
            mtb_prover_timeout_secs: 30,
            batch_size:              3,
        };
        let mtb = Prover::new(&options).unwrap();
        let mut input_data = get_default_proof_input();
        let identities = extract_identities_from(&input_data);
        input_data.post_root = U256::from(2);

        let prover_result = mtb
            .generate_proof(
                input_data.start_index,
                input_data.pre_root,
                input_data.post_root,
                identities,
            )
            .await;

        mock_service.stop();
        assert!(prover_result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn prover_should_error_if_batch_size_wrong() -> anyhow::Result<()> {
        let options = Options {
            mtb_prover_url:          "http://localhost:3002".into(),
            mtb_prover_timeout_secs: 30,
            batch_size:              10,
        };
        let mtb = Prover::new(&options).unwrap();
        let input_data = get_default_proof_input();
        let identities = extract_identities_from(&input_data);

        let prover_result = mtb
            .generate_proof(
                input_data.start_index,
                input_data.pre_root,
                input_data.post_root,
                identities,
            )
            .await;

        assert!(prover_result.is_err());
        assert_eq!(
            prover_result.unwrap_err().to_string(),
            anyhow::Error::msg("Provided batch does not match prover batch size.").to_string()
        );

        Ok(())
    }

    #[test]
    fn compute_input_hash_should_succeed() {
        let input = get_default_proof_input();

        assert_eq!(
            compute_input_hash(
                input.start_index,
                input.pre_root,
                input.post_root,
                &input.identity_commitments
            ),
            input.input_hash
        );
    }

    #[test]
    fn proof_input_should_serde() {
        let expected_data: ProofInput = serde_json::from_str(EXPECTED_JSON).unwrap();
        let proof_input = get_default_proof_input();

        assert_eq!(proof_input, expected_data);
    }

    fn extract_identities_from(proof_input: &ProofInput) -> Vec<Identity> {
        proof_input
            .identity_commitments
            .iter()
            .zip(&proof_input.merkle_proofs)
            .map(|(comm, prf)| Identity::new(*comm, prf.clone()))
            .collect()
    }

    pub fn get_default_proof_output() -> Proof {
        Proof::from([
            "0x12bba8b5a46139c819d83544f024828ece34f4f46be933a377a07c1904e96ec4".into(),
            "0x112c8d7c63b6c431cef23e9c0d9ffff39d1d660f514030d4f2787960b437a1d5".into(),
            "0x2413396a2af3add6fbe8137cfe7657917e31a5cdab0b7d1d645bd5eeb47ba601".into(),
            "0x1ad029539528b32ba70964ce43dbf9bba2501cdb3aaa04e4d58982e2f6c34752".into(),
            "0x5bb975296032b135458bd49f92d5e9d363367804440d4692708de92e887cf17".into(),
            "0x14932600f53a1ceb11d79a7bdd9688a2f8d1919176f257f132587b2b3274c41e".into(),
            "0x13d7b19c7b67bf5d3adf2ac2d3885fd5d49435b6069c0656939cd1fb7bef9dc9".into(),
            "0x142e14f90c49c79b4edf5f6b7acbcdb0b0f376a4311fc036f1006679bd53ca9e".into(),
        ])
    }

    fn get_default_proof_input() -> ProofInput {
        let start_index: u32 = 0;
        let pre_root: U256 =
            "0x1b7201da72494f1e28717ad1a52eb469f95892f957713533de6175e5da190af2".into();
        let post_root: U256 =
            "0x7b248024e18c30f6c8a6c63dad3748d72cd13d1197bfd79a1323216d6ac6e99".into();
        let identities: Vec<U256> = vec!["0x1".into(), "0x2".into(), "0x3".into()];
        let merkle_proofs: Vec<Vec<U256>> = vec![
            vec![
                "0x0".into(),
                "0x2098f5fb9e239eab3ceac3f27b81e481dc3124d55ffed523a839ee8446b64864".into(),
                "0x1069673dcdb12263df301a6ff584a7ec261a44cb9dc68df067a4774460b1f1e1".into(),
                "0x18f43331537ee2af2e3d758d50f72106467c6eea50371dd528d57eb2b856d238".into(),
                "0x7f9d837cb17b0d36320ffe93ba52345f1b728571a568265caac97559dbc952a".into(),
                "0x2b94cf5e8746b3f5c9631f4c5df32907a699c58c94b2ad4d7b5cec1639183f55".into(),
                "0x2dee93c5a666459646ea7d22cca9e1bcfed71e6951b953611d11dda32ea09d78".into(),
                "0x78295e5a22b84e982cf601eb639597b8b0515a88cb5ac7fa8a4aabe3c87349d".into(),
                "0x2fa5e5f18f6027a6501bec864564472a616b2e274a41211a444cbe3a99f3cc61".into(),
                "0xe884376d0d8fd21ecb780389e941f66e45e7acce3e228ab3e2156a614fcd747".into(),
            ],
            vec![
                "0x1".into(),
                "0x2098f5fb9e239eab3ceac3f27b81e481dc3124d55ffed523a839ee8446b64864".into(),
                "0x1069673dcdb12263df301a6ff584a7ec261a44cb9dc68df067a4774460b1f1e1".into(),
                "0x18f43331537ee2af2e3d758d50f72106467c6eea50371dd528d57eb2b856d238".into(),
                "0x7f9d837cb17b0d36320ffe93ba52345f1b728571a568265caac97559dbc952a".into(),
                "0x2b94cf5e8746b3f5c9631f4c5df32907a699c58c94b2ad4d7b5cec1639183f55".into(),
                "0x2dee93c5a666459646ea7d22cca9e1bcfed71e6951b953611d11dda32ea09d78".into(),
                "0x78295e5a22b84e982cf601eb639597b8b0515a88cb5ac7fa8a4aabe3c87349d".into(),
                "0x2fa5e5f18f6027a6501bec864564472a616b2e274a41211a444cbe3a99f3cc61".into(),
                "0xe884376d0d8fd21ecb780389e941f66e45e7acce3e228ab3e2156a614fcd747".into(),
            ],
            vec![
                "0x0".into(),
                "0x115cc0f5e7d690413df64c6b9662e9cf2a3617f2743245519e19607a4417189a".into(),
                "0x1069673dcdb12263df301a6ff584a7ec261a44cb9dc68df067a4774460b1f1e1".into(),
                "0x18f43331537ee2af2e3d758d50f72106467c6eea50371dd528d57eb2b856d238".into(),
                "0x7f9d837cb17b0d36320ffe93ba52345f1b728571a568265caac97559dbc952a".into(),
                "0x2b94cf5e8746b3f5c9631f4c5df32907a699c58c94b2ad4d7b5cec1639183f55".into(),
                "0x2dee93c5a666459646ea7d22cca9e1bcfed71e6951b953611d11dda32ea09d78".into(),
                "0x78295e5a22b84e982cf601eb639597b8b0515a88cb5ac7fa8a4aabe3c87349d".into(),
                "0x2fa5e5f18f6027a6501bec864564472a616b2e274a41211a444cbe3a99f3cc61".into(),
                "0xe884376d0d8fd21ecb780389e941f66e45e7acce3e228ab3e2156a614fcd747".into(),
            ],
        ];
        let input_hash: U256 =
            "0xa2d9c54a0aecf0f2aeb502c4a14ac45209d636986294c5e3168a54a7f143b1d8".into();

        ProofInput {
            input_hash,
            start_index,
            pre_root,
            post_root,
            identity_commitments: identities,
            merkle_proofs,
        }
    }

    const EXPECTED_JSON: &str = r#"{
  "inputHash": "0xa2d9c54a0aecf0f2aeb502c4a14ac45209d636986294c5e3168a54a7f143b1d8",
  "startIndex": 0,
  "preRoot": "0x1b7201da72494f1e28717ad1a52eb469f95892f957713533de6175e5da190af2",
  "postRoot": "0x7b248024e18c30f6c8a6c63dad3748d72cd13d1197bfd79a1323216d6ac6e99",
  "identityCommitments": [
    "0x1",
    "0x2",
    "0x3"
  ],
  "merkleProofs": [
    [
      "0x0",
      "0x2098f5fb9e239eab3ceac3f27b81e481dc3124d55ffed523a839ee8446b64864",
      "0x1069673dcdb12263df301a6ff584a7ec261a44cb9dc68df067a4774460b1f1e1",
      "0x18f43331537ee2af2e3d758d50f72106467c6eea50371dd528d57eb2b856d238",
      "0x7f9d837cb17b0d36320ffe93ba52345f1b728571a568265caac97559dbc952a",
      "0x2b94cf5e8746b3f5c9631f4c5df32907a699c58c94b2ad4d7b5cec1639183f55",
      "0x2dee93c5a666459646ea7d22cca9e1bcfed71e6951b953611d11dda32ea09d78",
      "0x78295e5a22b84e982cf601eb639597b8b0515a88cb5ac7fa8a4aabe3c87349d",
      "0x2fa5e5f18f6027a6501bec864564472a616b2e274a41211a444cbe3a99f3cc61",
      "0xe884376d0d8fd21ecb780389e941f66e45e7acce3e228ab3e2156a614fcd747"
    ],
    [
      "0x1",
      "0x2098f5fb9e239eab3ceac3f27b81e481dc3124d55ffed523a839ee8446b64864",
      "0x1069673dcdb12263df301a6ff584a7ec261a44cb9dc68df067a4774460b1f1e1",
      "0x18f43331537ee2af2e3d758d50f72106467c6eea50371dd528d57eb2b856d238",
      "0x7f9d837cb17b0d36320ffe93ba52345f1b728571a568265caac97559dbc952a",
      "0x2b94cf5e8746b3f5c9631f4c5df32907a699c58c94b2ad4d7b5cec1639183f55",
      "0x2dee93c5a666459646ea7d22cca9e1bcfed71e6951b953611d11dda32ea09d78",
      "0x78295e5a22b84e982cf601eb639597b8b0515a88cb5ac7fa8a4aabe3c87349d",
      "0x2fa5e5f18f6027a6501bec864564472a616b2e274a41211a444cbe3a99f3cc61",
      "0xe884376d0d8fd21ecb780389e941f66e45e7acce3e228ab3e2156a614fcd747"
    ],
    [
      "0x0",
      "0x115cc0f5e7d690413df64c6b9662e9cf2a3617f2743245519e19607a4417189a",
      "0x1069673dcdb12263df301a6ff584a7ec261a44cb9dc68df067a4774460b1f1e1",
      "0x18f43331537ee2af2e3d758d50f72106467c6eea50371dd528d57eb2b856d238",
      "0x7f9d837cb17b0d36320ffe93ba52345f1b728571a568265caac97559dbc952a",
      "0x2b94cf5e8746b3f5c9631f4c5df32907a699c58c94b2ad4d7b5cec1639183f55",
      "0x2dee93c5a666459646ea7d22cca9e1bcfed71e6951b953611d11dda32ea09d78",
      "0x78295e5a22b84e982cf601eb639597b8b0515a88cb5ac7fa8a4aabe3c87349d",
      "0x2fa5e5f18f6027a6501bec864564472a616b2e274a41211a444cbe3a99f3cc61",
      "0xe884376d0d8fd21ecb780389e941f66e45e7acce3e228ab3e2156a614fcd747"
    ]
  ]
}
"#;
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use axum::{routing::post, Json, Router};
    use axum_server::Handle;
    use std::net::SocketAddr;

    pub struct Service {
        server: Handle,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    #[allow(clippy::large_enum_variant)]
    enum ProveResponse {
        ProofSuccess(Proof),
        ProofFailure(ProverError),
    }

    impl Service {
        pub async fn new(url: String) -> anyhow::Result<Self> {
            let prove = |Json(payload): Json<ProofInput>| async move {
                match payload.post_root.div_mod(U256::from(2)) {
                    (_, y) if y != U256::zero() => {
                        Json(ProveResponse::ProofSuccess(test::get_default_proof_output()))
                    }
                    _ => {
                        let error = ProverError {
                            code:    "Oh no!".into(),
                            message: "Things went wrong.".into(),
                        };
                        Json(ProveResponse::ProofFailure(error))
                    }
                }
            };
            let app = Router::new().route("/prove", post(prove));

            let addr: SocketAddr = url.parse()?;
            let server = Handle::new();
            let serverside_handle = server.clone();
            let service = app.into_make_service();

            tokio::spawn(async move {
                axum_server::bind(addr)
                    .handle(serverside_handle)
                    .serve(service)
                    .await
                    .unwrap();
            });

            let service = Self { server };
            Ok(service)
        }

        pub fn stop(self) {
            self.server.shutdown();
        }
    }
}