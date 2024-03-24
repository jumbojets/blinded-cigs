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

//! This module defines [Session] and [Segment] which provides a way to share
//! execution traces between the execution phase and the proving phase.

use alloc::collections::BTreeSet;
use std::{
    borrow::Borrow,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, ensure, Result};
use human_repr::HumanCount;
use risc0_binfmt::{MemoryImage, SystemState};
use risc0_zkvm_platform::WORD_SIZE;
use serde::{Deserialize, Serialize};

use crate::{
    host::server::exec::executor::SyscallRecord, sha::Digest, Assumption, Assumptions, ExitCode,
    Journal, Output, ReceiptClaim,
};

#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct PageFaults {
    pub(crate) reads: BTreeSet<u32>,
    pub(crate) writes: BTreeSet<u32>,
}

/// The execution trace of a program.
///
/// The record of memory transactions of an execution that starts from an
/// initial memory image (which includes the starting PC) and proceeds until
/// either a sys_halt or a sys_pause syscall is encountered. This record is
/// stored as a vector of [Segment]s.
#[derive(Serialize, Deserialize)]
pub struct Session {
    /// The constituent [Segment]s of the Session. The final [Segment] will have
    /// an [ExitCode] of [Halted](ExitCode::Halted), [Paused](ExitCode::Paused),
    /// or [SessionLimit](ExitCode::SessionLimit), and all other [Segment]s (if
    /// any) will have [ExitCode::SystemSplit].
    pub segments: Vec<Box<dyn SegmentRef>>,

    /// The data publicly committed by the guest program.
    pub journal: Option<Journal>,

    /// The [ExitCode] of the session.
    pub exit_code: ExitCode,

    /// The final [MemoryImage] at the end of execution.
    pub post_image: MemoryImage,

    /// The list of assumptions made by the guest and resolved by the host.
    pub assumptions: Vec<Assumption>,

    /// The hooks to be called during the proving phase.
    #[serde(skip)]
    pub hooks: Vec<Box<dyn SessionEvents>>,
}

/// A reference to a [Segment].
///
/// This allows implementors to determine the best way to represent this in an
/// pluggable manner. See the [SimpleSegmentRef] for a very basic
/// implmentation.
#[typetag::serde(tag = "type")]
pub trait SegmentRef: Send {
    /// Resolve this reference into an actual [Segment].
    fn resolve(&self) -> Result<Segment>;
}

/// The execution trace of a portion of a program.
///
/// The record of memory transactions of an execution that starts from an
/// initial memory image, and proceeds until terminated by the system or user.
/// This represents a chunk of execution work that will be proven in a single
/// call to the ZKP system. It does not necessarily represent an entire program;
/// see [Session] for tracking memory transactions until a user-requested
/// termination.
#[derive(Clone, Serialize, Deserialize)]
pub struct Segment {
    pub(crate) pre_image: Box<MemoryImage>,
    // NOTE: segment.post_state is NOT EQUAL to segment.get_claim()?.post. This is because the
    // post SystemState on the ReceiptClaim struct has a PC that is shifted forward by 4.
    pub(crate) post_state: SystemState,
    pub(crate) output: Option<Output>,
    pub(crate) faults: PageFaults,
    pub(crate) syscalls: Vec<SyscallRecord>,
    pub(crate) split_insn: Option<u32>,
    pub(crate) exit_code: ExitCode,

    /// The number of cycles in powers of 2.
    pub po2: u32,

    /// The index of this [Segment] within the [Session]
    pub index: u32,

    /// The number of user cycles without any overhead for continuations or po2
    /// padding.
    pub cycles: u32,
}

/// The Events of [Session]
pub trait SessionEvents {
    /// Fired before the proving of a segment starts.
    #[allow(unused)]
    fn on_pre_prove_segment(&self, segment: &Segment) {}

    /// Fired after the proving of a segment ends.
    #[allow(unused)]
    fn on_post_prove_segment(&self, segment: &Segment) {}
}

impl Session {
    /// Construct a new [Session] from its constituent components.
    pub fn new(
        segments: Vec<Box<dyn SegmentRef>>,
        journal: Option<Vec<u8>>,
        exit_code: ExitCode,
        post_image: MemoryImage,
        assumptions: Vec<Assumption>,
    ) -> Self {
        Self {
            segments,
            journal: journal.map(|x| Journal::new(x)),
            exit_code,
            post_image,
            assumptions,
            hooks: Vec::new(),
        }
    }

    /// A convenience method that resolves all [SegmentRef]s and returns the
    /// associated [Segment]s.
    pub fn resolve(&self) -> Result<Vec<Segment>> {
        self.segments
            .iter()
            .map(|segment_ref| segment_ref.resolve())
            .collect()
    }

    /// Add a hook to be called during the proving phase.
    pub fn add_hook<E: SessionEvents + 'static>(&mut self, hook: E) {
        self.hooks.push(Box::new(hook));
    }

    /// Calculate for the [ReceiptClaim] associated with this [Session]. The
    /// [ReceiptClaim] is the claim that will be proven if this [Session]
    /// is passed to the [crate::Prover].
    pub fn get_claim(&self) -> Result<ReceiptClaim> {
        let first_segment = &self
            .segments
            .first()
            .ok_or_else(|| anyhow!("session has no segments"))?
            .resolve()?;
        let last_segment = &self
            .segments
            .last()
            .ok_or_else(|| anyhow!("session has no segments"))?
            .resolve()?;

        // Construct the Output struct for the session, checking internal consistency.
        // NOTE: The Session output if distinct from the final Segment output because in the
        // Session output any proven assumptions are not included.
        let output = if self.exit_code.expects_output() {
            self.journal
                .as_ref()
                .map(|journal| -> Result<_> {
                    Ok(Output {
                        journal: journal.bytes.clone().into(),
                        assumptions: Assumptions(
                            self.assumptions
                                .iter()
                                .filter_map(|a| match a {
                                    Assumption::Proven(_) => None,
                                    Assumption::Unresolved(r) => Some(r.clone()),
                                })
                                .collect::<Vec<_>>(),
                        )
                        .into(),
                    })
                })
                .transpose()?
        } else {
            ensure!(
                self.journal.is_none(),
                "Session with exit code {:?} has a journal",
                self.exit_code
            );
            ensure!(
                self.assumptions.is_empty(),
                "Session with exit code {:?} has encoded assumptions",
                self.exit_code
            );
            None
        };

        // NOTE: When a segment ends in a Halted(_) state, it may not update the post state
        // digest. As a result, it will be the same are the pre_image. All other exit codes require
        // the post state digest to reflect the final memory state.
        // NOTE: The PC on the the post state is stored "+ 4". See ReceiptClaim for more detail.
        let post_state = SystemState {
            pc: self
                .post_image
                .pc
                .checked_add(WORD_SIZE as u32)
                .ok_or(anyhow!("invalid pc in session post image"))?,
            merkle_root: match self.exit_code {
                ExitCode::Halted(_) => last_segment.pre_image.compute_root_hash()?,
                _ => self.post_image.compute_root_hash()?,
            },
        };

        Ok(ReceiptClaim {
            pre: SystemState::from(first_segment.pre_image.borrow()).into(),
            post: post_state.into(),
            exit_code: self.exit_code,
            input: Digest::ZERO,
            output: output.into(),
        })
    }

    /// Report cycle information for this [Session].
    ///
    /// Returns a tuple `(x, y)` where:
    /// * `x`: Total number of cycles that a prover experiences. This includes
    ///   overhead associated with continuations and padding up to the nearest
    ///   power of 2.
    /// * `y`: Total number of cycles used for executing user instructions.
    pub fn get_cycles(&self) -> Result<(u64, u64)> {
        let segments = self.resolve()?;
        Ok(segments
            .iter()
            .fold((0, 0), |(total_cycles, user_cycles), segment| {
                (
                    total_cycles + (1 << segment.po2),
                    user_cycles + segment.cycles as u64,
                )
            }))
    }

    /// Log cycle information for this [Session].
    ///
    /// This logs the total and user cycles for this [Session] at the INFO level.
    pub fn log(&self) -> anyhow::Result<()> {
        // TODO: Refactor this call to `get_cycles` to avoid the costly `resolve` call.
        // reference: <https://github.com/risc0/risc0/pull/1276#issuecomment-1877792024>
        let (total_prover_cycles, user_instruction_cycles) = self.get_cycles()?;
        let cycles_used_ratio = user_instruction_cycles as f64 / total_prover_cycles as f64 * 100.0;

        tracing::info!(
            "number of segments: {}",
            self.segments.len().human_count_bare()
        );
        tracing::info!(
            "total prover cycles: {}",
            total_prover_cycles.human_count_bare()
        );
        tracing::info!(
            "user instruction cycles: {}",
            user_instruction_cycles.human_count_bare()
        );
        tracing::info!(
            "cycle efficiency: {}%",
            cycles_used_ratio.human_count_bare()
        );

        Ok(())
    }
}

impl Segment {
    /// Create a new [Segment] from its constituent components.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        pre_image: Box<MemoryImage>,
        post_state: SystemState,
        output: Option<Output>,
        faults: PageFaults,
        syscalls: Vec<SyscallRecord>,
        exit_code: ExitCode,
        split_insn: Option<u32>,
        po2: u32,
        index: u32,
        cycles: u32,
    ) -> Self {
        tracing::debug!("segment[{index}]> reads: {}, writes: {}, exit_code: {exit_code:?}, split_insn: {split_insn:?}, po2: {po2}, cycles: {cycles}",
            faults.reads.len(),
            faults.writes.len(),
        );
        Self {
            pre_image,
            post_state,
            output,
            faults,
            syscalls,
            exit_code,
            split_insn,
            po2,
            index,
            cycles,
        }
    }

    /// Calculate for the [ReceiptClaim] associated with this [Segment]. The
    /// [ReceiptClaim] is the claim that will be proven if this [Segment]
    /// is passed to the [crate::Prover].
    pub fn get_claim(&self) -> Result<ReceiptClaim> {
        // NOTE: When a segment ends in a Halted(_) state, it may not update the post state
        // digest. As a result, it will be the same are the pre_image. All other exit codes require
        // the post state digest to reflect the final memory state.
        // NOTE: The PC on the the post state is stored "+ 4". See ReceiptClaim for more detail.
        let post_state = SystemState {
            pc: self
                .post_state
                .pc
                .checked_add(WORD_SIZE as u32)
                .ok_or(anyhow!("invalid pc in segment post state"))?,
            merkle_root: match self.exit_code {
                ExitCode::Halted(_) => self.pre_image.compute_root_hash()?,
                _ => self.post_state.merkle_root.clone(),
            },
        };

        Ok(ReceiptClaim {
            pre: SystemState::from(&*self.pre_image).into(),
            post: post_state.into(),
            exit_code: self.exit_code,
            input: Digest::ZERO,
            output: self.output.clone().into(),
        })
    }
}

/// A very basic implementation of a [SegmentRef].
///
/// The [Segment] itself is stored in this implementation.
#[derive(Clone, Serialize, Deserialize)]
pub struct SimpleSegmentRef {
    segment: Segment,
}

#[typetag::serde]
impl SegmentRef for SimpleSegmentRef {
    fn resolve(&self) -> Result<Segment> {
        Ok(self.segment.clone())
    }
}

impl SimpleSegmentRef {
    /// Construct a [SimpleSegmentRef] with the specified [Segment].
    pub fn new(segment: Segment) -> Self {
        Self { segment }
    }
}

/// A basic implementation of a [SegmentRef] that saves the segment to a file
///
/// The [Segment] is stored in a user-specified file in this implementation,
/// and the SegmentRef holds the filename.
///
/// There is an example of using [FileSegmentRef] in our [EVM example][1]
///
/// [1]: https://github.com/risc0/risc0/blob/main/examples/zkevm-demo/src/main.rs
#[derive(Clone, Serialize, Deserialize)]
pub struct FileSegmentRef {
    path: PathBuf,
}

#[typetag::serde]
impl SegmentRef for FileSegmentRef {
    fn resolve(&self) -> Result<Segment> {
        let mut contents = Vec::new();
        let mut file = File::open(&self.path)?;
        file.read_to_end(&mut contents)?;
        let segment: Segment = bincode::deserialize(&contents)?;
        Ok(segment)
    }
}

impl FileSegmentRef {
    /// Construct a [FileSegmentRef]
    ///
    /// This builds a FileSegmentRef that stores `segment` in a file at `path`.
    pub fn new(segment: &Segment, path: &Path) -> Result<Self> {
        let path = path.join(format!("{}.bincode", segment.index));
        let mut file = File::create(&path)?;
        let contents = bincode::serialize(&segment)?;
        file.write_all(&contents)?;
        Ok(Self { path })
    }
}
