#![doc = include_str!("../README.md")]

use hello_world_methods::{MULTIPLY_ID, MULTIPLY_ELF};
use risc0_zkvm::{default_prover, ExecutorEnv, Receipt, sha::Digest};
use blindr_common::{Transaction, Auth, Constraint};

type BlindedMessage = ();

pub fn prove(message: &Transaction, auth: &Auth, constraint: &Constraint) -> (Receipt, BlindedMessage, Digest) {
    let env = ExecutorEnv::builder()
        .write(&message)
        .unwrap()
        .write(&auth)
        .unwrap()
        .write(&constraint)
        .unwrap()
        .build()
        .unwrap();
    
    let prover = default_prover();

    let receipt = prover.prove(env, MULTIPLY_ELF).unwrap();

    let (blinded_message, hashed_constraint) = receipt.journal.decode().unwrap();

    (receipt, blinded_message, hashed_constraint)
}

pub fn verify(receipt: &Receipt) { // TODO: dont we need blinded message and hashed constraint?
    receipt.verify(MULTIPLY_ID).expect("Code you have proven should successfully verify; did you specify the correct image ID?");
}
