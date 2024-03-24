// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use bonsai_sdk::alpha::Client;

use super::Prover;
use crate::{compute_image_id, sha::Digestible, ExecutorEnv, ProverOpts, Receipt, VerifierContext};

/// An implementation of a [Prover] that runs proof workloads via Bonsai.
///
/// Requires `BONSAI_API_URL` and `BONSAI_API_KEY` environment variables to
/// submit proving sessions to Bonsai.
pub struct BonsaiProver {
    name: String,
}

impl BonsaiProver {
    /// Construct a [BonsaiProver].
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}

impl Prover for BonsaiProver {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    fn prove_with_ctx(
        &self,
        env: ExecutorEnv<'_>,
        ctx: &VerifierContext,
        elf: &[u8],
        opts: &ProverOpts,
    ) -> Result<Receipt> {
        let client = Client::from_env(crate::VERSION)?;

        // Compute the ImageID and upload the ELF binary
        let image_id = compute_image_id(elf)?;
        let image_id_hex = hex::encode(image_id.clone());
        client.upload_img(&image_id_hex, elf.to_vec())?;

        // upload input data
        let input_id = client.upload_input(env.input)?;

        // upload receipts
        let mut receipts_ids: Vec<String> = vec![];
        for assumption in &env.assumptions.borrow().cached {
            let serialized_receipt = match assumption {
                crate::Assumption::Proven(receipt) => bincode::serialize(receipt)?,
                crate::Assumption::Unresolved(_) => bail!("Only proven receipts can be uploaded."), //TODO: improve the message
            };
            let receipt_id = client.upload_receipt(serialized_receipt)?;
            receipts_ids.push(receipt_id);
        }

        // While this is the executor, we want to start a session on the bonsai prover.
        // By doing so, we can return a session ID so that the prover can use it to
        // retrieve the receipt.
        let session = client.create_session(image_id_hex, input_id, receipts_ids)?;
        tracing::debug!("Bonsai proving SessionID: {}", session.uuid);

        loop {
            // The session has already been started in the executor. Poll bonsai to check if
            // the proof request succeeded.
            let res = session.status(&client)?;
            if res.status == "RUNNING" {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            if res.status == "SUCCEEDED" {
                // Download the receipt, containing the output
                let receipt_url = res
                    .receipt_url
                    .ok_or(anyhow!("API error, missing receipt on completed session"))?;

                let receipt_buf = client.download(&receipt_url)?;
                let receipt: Receipt = bincode::deserialize(&receipt_buf)?;

                if opts.prove_guest_errors {
                    receipt.verify_integrity_with_context(ctx)?;
                    ensure!(
                        receipt.get_claim()?.pre.digest() == image_id,
                        "received unexpected image ID: expected {}, found {}",
                        hex::encode(&image_id),
                        hex::encode(&receipt.get_claim()?.pre.digest())
                    );
                } else {
                    receipt.verify_with_context(ctx, image_id)?;
                }
                return Ok(receipt);
            } else {
                bail!("Bonsai prover workflow exited: {}", res.status);
            }
        }
    }
}
