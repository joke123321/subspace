// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! TODO
#![warn(missing_docs)]
#![allow(clippy::all)]

pub mod collation_generation;

use std::{
	collections::{hash_map, HashMap},
	fmt::Debug,
	sync::Arc,
};

use futures::{select, stream::FusedStream, StreamExt};
use lru::LruCache;
use smallvec::SmallVec;
use std::fmt;

use sc_client_api::{BlockBackend, BlockImportNotification, BlockchainEvents};
use sp_api::{ApiError, ProvideRuntimeApi};
use sp_blockchain::HeaderBackend;

use crate::collation_generation::CollationGenerationMessage;

use cirrus_node_primitives::{CollationGenerationConfig, ExecutorSlotInfo};
use sp_executor::{BundleEquivocationProof, ExecutorApi, FraudProof, InvalidTransactionProof};
use subspace_runtime_primitives::{opaque::Block, BlockNumber, Hash};

/// How many slots are stack-reserved for active leaves updates
///
/// If there are fewer than this number of slots, then we've wasted some stack space.
/// If there are greater than this number of slots, then we fall back to a heap vector.
const ACTIVE_LEAVES_SMALLVEC_CAPACITY: usize = 8;

/// Activated leaf.
#[derive(Debug, Clone)]
pub struct ActivatedLeaf {
	/// The block hash.
	pub hash: Hash,
	/// The block number.
	pub number: BlockNumber,
}

/// Changes in the set of active leaves: the parachain heads which we care to work on.
///
/// Note that the activated and deactivated fields indicate deltas, not complete sets.
#[derive(Clone, Default)]
pub struct ActiveLeavesUpdate {
	/// New relay chain block of interest.
	pub activated: Option<ActivatedLeaf>,
	/// Relay chain block hashes no longer of interest.
	pub deactivated: SmallVec<[Hash; ACTIVE_LEAVES_SMALLVEC_CAPACITY]>,
}

impl ActiveLeavesUpdate {
	/// Create a `ActiveLeavesUpdate` with a single activated hash
	pub fn start_work(activated: ActivatedLeaf) -> Self {
		Self { activated: Some(activated), ..Default::default() }
	}

	/// Create a `ActiveLeavesUpdate` with a single deactivated hash
	pub fn stop_work(hash: Hash) -> Self {
		Self { deactivated: [hash][..].into(), ..Default::default() }
	}

	/// Is this update empty and doesn't contain any information?
	pub fn is_empty(&self) -> bool {
		self.activated.is_none() && self.deactivated.is_empty()
	}
}

impl PartialEq for ActiveLeavesUpdate {
	/// Equality for `ActiveLeavesUpdate` doesn't imply bitwise equality.
	///
	/// Instead, it means equality when `activated` and `deactivated` are considered as sets.
	fn eq(&self, other: &Self) -> bool {
		self.activated.as_ref().map(|a| a.hash) == other.activated.as_ref().map(|a| a.hash) &&
			self.deactivated.len() == other.deactivated.len() &&
			self.deactivated.iter().all(|a| other.deactivated.contains(a))
	}
}

impl fmt::Debug for ActiveLeavesUpdate {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ActiveLeavesUpdate")
			.field("activated", &self.activated)
			.field("deactivated", &self.deactivated)
			.finish()
	}
}

/// Store 2 days worth of blocks, not accounting for forks,
/// in the LRU cache. Assumes a 6-second block time.
pub const KNOWN_LEAVES_CACHE_SIZE: usize = 2 * 24 * 3600 / 6;

/// A handle used to communicate with the [`Overseer`].
///
/// [`Overseer`]: struct.Overseer.html
#[derive(Clone)]
pub struct Handle(OverseerHandle);

impl Handle {
	/// Create a new [`Handle`].
	fn new(raw: OverseerHandle) -> Self {
		Self(raw)
	}

	/// Inform the `Overseer` that that some block was imported.
	async fn block_imported(&mut self, block: BlockInfo) {
		self.send_and_log_error(Event::BlockImported(block)).await
	}

	/// Send some message to one of the `Subsystem`s.
	async fn send_msg(&mut self, msg: CollationGenerationMessage) {
		self.send_and_log_error(Event::MsgToSubsystem(msg)).await
	}

	/// Inform the `Overseer` that a new slot was triggered.
	async fn slot_arrived(&mut self, slot_info: ExecutorSlotInfo) {
		self.send_and_log_error(Event::NewSlot(slot_info)).await
	}

	/// Most basic operation, to stop a server.
	async fn send_and_log_error(&mut self, event: Event) {
		if self.0.send(event).await.is_err() {
			tracing::info!(target: LOG_TARGET, "Failed to send an event to Overseer");
		}
	}

	/// TODO
	pub async fn initialize(&mut self, config: CollationGenerationConfig) {
		self.send_msg(CollationGenerationMessage::Initialize(config)).await;
	}

	/// TODO
	pub async fn submit_bundle_equivocation_proof(
		&mut self,
		bundle_equivocation_proof: BundleEquivocationProof,
	) {
		self.send_msg(CollationGenerationMessage::BundleEquivocationProof(
			bundle_equivocation_proof,
		))
		.await
	}

	/// TODO
	pub async fn submit_fraud_proof(&mut self, fraud_proof: FraudProof) {
		self.send_msg(CollationGenerationMessage::FraudProof(fraud_proof)).await;
	}

	/// TODO
	pub async fn submit_invalid_transaction_proof(
		&mut self,
		invalid_transaction_proof: InvalidTransactionProof,
	) {
		self.send_msg(CollationGenerationMessage::InvalidTransactionProof(
			invalid_transaction_proof,
		))
		.await;
	}
}

/// An event telling the `Overseer` on the particular block
/// that has been imported or finalized.
///
/// This structure exists solely for the purposes of decoupling
/// `Overseer` code from the client code and the necessity to call
/// `HeaderBackend::block_number_from_id()`.
#[derive(Debug, Clone)]
pub struct BlockInfo {
	/// hash of the block.
	pub hash: Hash,
	/// hash of the parent block.
	pub parent_hash: Hash,
	/// block's number.
	pub number: BlockNumber,
}

impl From<BlockImportNotification<Block>> for BlockInfo {
	fn from(n: BlockImportNotification<Block>) -> Self {
		BlockInfo { hash: n.hash, parent_hash: n.header.parent_hash, number: n.header.number }
	}
}

/// An event from outside the overseer scope, such
/// as the substrate framework or user interaction.
pub enum Event {
	/// A new block was imported.
	BlockImported(BlockInfo),
	/// A new slot arrived.
	NewSlot(ExecutorSlotInfo),
	/// Message as sent to a subsystem.
	MsgToSubsystem(CollationGenerationMessage),
}

/// Glues together the [`Overseer`] and `BlockchainEvents` by forwarding
/// import and finality notifications into the [`OverseerHandle`].
pub async fn forward_events<P: BlockchainEvents<Block>>(
	client: Arc<P>,
	mut slots: impl FusedStream<Item = ExecutorSlotInfo> + Unpin,
	mut handle: Handle,
) {
	let mut imports = client.import_notification_stream();

	loop {
		select! {
			i = imports.next() => {
				match i {
					Some(block) => {
						handle.block_imported(block.into()).await;
					}
					None => break,
				}
			},
			s = slots.next() => {
				match s {
					Some(executor_slot_info) => {
						handle.slot_arrived(executor_slot_info).await;
					}
					None => break,
				}
			}
			complete => break,
		}
	}
}

/// Capacity of a signal channel between a subsystem and the overseer.
const SIGNAL_CHANNEL_CAPACITY: usize = 64usize;
/// The log target tag.
const LOG_TARGET: &str = "overseer";
/// The overseer.
pub struct Overseer<Client> {
	/// A subsystem instance.
	collation_generation: crate::collation_generation::CollationGenerationSubsystem<Client>,
	/// A user specified addendum field.
	leaves: Vec<(Hash, BlockNumber)>,
	/// A user specified addendum field.
	active_leaves: HashMap<Hash, BlockNumber>,
	/// A user specified addendum field.
	known_leaves: LruCache<Hash, ()>,
	/// Events that are sent to the overseer from the outside world.
	events_rx: metered::MeteredReceiver<Event>,
}
/// Handle for an overseer.
type OverseerHandle = metered::MeteredSender<Event>;

impl<Client> Overseer<Client>
where
	Client: HeaderBackend<Block>
		+ BlockBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ 'static
		+ Sync,
	Client::Api: ExecutorApi<Block>,
{
	/// Create a new overseer.
	pub fn new(
		collation_generation: crate::collation_generation::CollationGenerationSubsystem<Client>,
		leaves: Vec<(Hash, BlockNumber)>,
		active_leaves: HashMap<Hash, BlockNumber>,
		known_leaves: LruCache<Hash, ()>,
	) -> (Self, Handle) {
		let (handle, events_rx) = metered::channel(SIGNAL_CHANNEL_CAPACITY);
		let overseer =
			Overseer { collation_generation, leaves, active_leaves, known_leaves, events_rx };
		(overseer, Handle::new(handle))
	}

	/// Run the `Overseer`.
	pub async fn run(mut self) -> Result<(), ApiError> {
		// Notify about active leaves on startup before starting the loop
		for (hash, number) in std::mem::take(&mut self.leaves) {
			let _ = self.active_leaves.insert(hash, number);
			self.known_leaves.put(hash, ());
			let update = ActiveLeavesUpdate::start_work(ActivatedLeaf { hash, number });
			if let Err(error) = self.collation_generation.update_active_leaves(update).await {
				tracing::error!(
					target: LOG_TARGET,
					"Collation generation processing error: {error}"
				);
			}
		}

		while let Some(msg) = self.events_rx.next().await {
			match msg {
				Event::MsgToSubsystem(message) => {
					if let Err(error) = self.collation_generation.handle_message(message).await {
						tracing::error!(
							target: LOG_TARGET,
							"Collation generation processing error: {error}"
						);
					}
				},
				Event::BlockImported(block) => {
					self.block_imported(block).await?;
				},
				Event::NewSlot(slot_info) => {
					if let Err(error) = self.collation_generation.update_new_slot(slot_info).await {
						tracing::error!(
							target: LOG_TARGET,
							"Collation generation processing error: {error}"
						);
					}
				},
			}
		}

		Ok(())
	}

	async fn block_imported(&mut self, block: BlockInfo) -> Result<(), ApiError> {
		match self.active_leaves.entry(block.hash) {
			hash_map::Entry::Vacant(entry) => entry.insert(block.number),
			hash_map::Entry::Occupied(entry) => {
				debug_assert_eq!(*entry.get(), block.number);
				return Ok(())
			},
		};

		self.known_leaves.put(block.hash, ());
		let mut update = ActiveLeavesUpdate::start_work(ActivatedLeaf {
			hash: block.hash,
			number: block.number,
		});

		if let Some(number) = self.active_leaves.remove(&block.parent_hash) {
			debug_assert_eq!(block.number.saturating_sub(1), number);
			update.deactivated.push(block.parent_hash);
		}

		if !update.is_empty() {
			if let Err(error) = self.collation_generation.update_active_leaves(update).await {
				tracing::error!(
					target: LOG_TARGET,
					"Collation generation processing error: {error}"
				);
			}
		}
		Ok(())
	}
}
