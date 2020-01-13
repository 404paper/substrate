// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Transaction pool primitives types & Runtime API.

use std::{
	collections::HashMap,
	hash::Hash,
	sync::Arc,
};
use futures::{
	Future, Stream,
	channel::mpsc,
};
use serde::{Deserialize, Serialize};
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Member},
	transaction_validity::{
		TransactionLongevity, TransactionPriority, TransactionTag,
	},
};

/// Transaction pool status.
#[derive(Debug)]
pub struct PoolStatus {
	/// Number of transactions in the ready queue.
	pub ready: usize,
	/// Sum of bytes of ready transaction encodings.
	pub ready_bytes: usize,
	/// Number of transactions in the future queue.
	pub future: usize,
	/// Sum of bytes of ready transaction encodings.
	pub future_bytes: usize,
}

impl PoolStatus {
	/// Returns true if the are no transactions in the pool.
	pub fn is_empty(&self) -> bool {
		self.ready == 0 && self.future == 0
	}
}

/// Possible transaction status events.
///
/// This events are being emitted by `TransactionPool` watchers,
/// which are also exposed over RPC.
///
/// The status events can be grouped based on their kinds as:
/// 1. Entering/Moving within the pool:
///		- `Future`
///		- `Ready`
/// 2. Inside `Ready` queue:
///		- `Broadcast`
/// 3. Leaving the pool:
///		- `InBlock`
///		- `Invalid`
///		- `Usurped`
///		- `Dropped`
///
/// The events will always be received in the order described above, however
/// there might be cases where transactions alternate between `Future` and `Ready`
/// pool, and are `Broadcast` in the meantime.
///
/// There is also only single event causing the transaction to leave the pool.
///
/// Note that there are conditions that may cause transactions to reappear in the pool.
/// 1. Due to possible forks, the transaction that ends up being in included
/// in one block, may later re-enter the pool or be marked as invalid.
/// 2. Transaction `Dropped` at one point, may later re-enter the pool if some other
/// transactions are removed.
/// 3. `Invalid` transaction may become valid at some point in the future.
/// (Note that runtimes are encouraged to use `UnknownValidity` to inform the pool about
/// such case).
///
/// However the user needs to re-subscribe to receive such notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionStatus<Hash, BlockHash> {
	/// Transaction is part of the future queue.
	Future,
	/// Transaction is part of the ready queue.
	Ready,
	/// The transaction has been broadcast to the given peers.
	Broadcast(Vec<String>),
	/// Transaction has been included in block with given hash.
	#[serde(rename = "finalized")] // See #4438
	InBlock(BlockHash),
	/// Transaction has been replaced in the pool, by another transaction
	/// that provides the same tags. (e.g. same (sender, nonce)).
	Usurped(Hash),
	/// Transaction has been dropped from the pool because of the limit.
	Dropped,
	/// Transaction is no longer valid in the current state.
	Invalid,
}

/// The stream of transaction events.
pub type TransactionStatusStream<Hash, BlockHash> = dyn Stream<Item=TransactionStatus<Hash, BlockHash>> + Send + Unpin;

/// The import notification event stream.
pub type ImportNotificationStream = mpsc::UnboundedReceiver<()>;

/// Transaction hash type for a pool.
pub type TxHash<P> = <P as TransactionPool>::Hash;
/// Block hash type for a pool.
pub type BlockHash<P> = <<P as TransactionPool>::Block as BlockT>::Hash;
/// Transaction type for a pool.
pub type TransactionFor<P> = <<P as TransactionPool>::Block as BlockT>::Extrinsic;
/// Type of transactions event stream for a pool.
pub type TransactionStatusStreamFor<P> = TransactionStatusStream<TxHash<P>, BlockHash<P>>;

/// In-pool transaction interface.
///
/// The pool is container of transactions that are implementing this trait.
/// See `sp_runtime::ValidTransaction` for details about every field.
pub trait InPoolTransaction {
	/// Transaction type.
	type Transaction;
	/// Transaction hash type.
	type Hash;

	/// Get the reference to the transaction data.
	fn data(&self) -> &Self::Transaction;
	/// Get hash of the transaction.
	fn hash(&self) -> &Self::Hash;
	/// Get priority of the transaction.
	fn priority(&self) -> &TransactionPriority;
	/// Get longevity of the transaction.
	fn longevity(&self) ->&TransactionLongevity;
	/// Get transaction dependencies.
	fn requires(&self) -> &[TransactionTag];
	/// Get tags that transaction provides.
	fn provides(&self) -> &[TransactionTag];
	/// Return a flag indicating if the transaction should be propagated to other peers.
	fn is_propagateable(&self) -> bool;
}

/// Transaction pool interface.
pub trait TransactionPool: Send + Sync {
	/// Block type.
	type Block: BlockT;
	/// Transaction hash type.
	type Hash: Hash + Eq + Member + Serialize;
	/// In-pool transaction type.
	type InPoolTransaction: InPoolTransaction<
		Transaction = TransactionFor<Self>,
		Hash = TxHash<Self>
	>;
	/// Error type.
	type Error: From<crate::error::Error> + crate::error::IntoPoolError;

	// Networking

	/// Returns a future that imports a bunch of unverified transactions to the pool.
	fn submit_at(
		&self,
		at: &BlockId<Self::Block>,
		xts: impl IntoIterator<Item=TransactionFor<Self>> + 'static,
	) -> Box<dyn Future<Output=Result<
		Vec<Result<TxHash<Self>, Self::Error>>,
		Self::Error
	>> + Send + Unpin>;

	/// Returns a future that imports one unverified transaction to the pool.
	fn submit_one(
		&self,
		at: &BlockId<Self::Block>,
		xt: TransactionFor<Self>,
	) -> Box<dyn Future<Output=Result<
		TxHash<Self>,
		Self::Error
	>> + Send + Unpin>;

	// RPC

	/// Returns a future that import a single transaction and starts to watch their progress in the pool.
	fn submit_and_watch(
		&self,
		at: &BlockId<Self::Block>,
		xt: TransactionFor<Self>,
	) -> Box<dyn Future<Output=Result<Box<TransactionStatusStreamFor<Self>>, Self::Error>> + Send + Unpin>;


	// Block production / Networking

	/// Get an iterator for ready transactions ordered by priority
	fn ready(&self) -> Box<dyn Iterator<Item=Arc<Self::InPoolTransaction>>>;


	// Block production

	/// Remove transactions identified by given hashes (and dependent transactions) from the pool.
	fn remove_invalid(&self, hashes: &[TxHash<Self>]) -> Vec<Arc<Self::InPoolTransaction>>;

	// logging

	/// Returns pool status.
	fn status(&self) -> PoolStatus;

	// logging / RPC / networking

	/// Return an event stream of transactions imported to the pool.
	fn import_notification_stream(&self) -> ImportNotificationStream;

	// networking

	/// Notify the pool about transactions broadcast.
	fn on_broadcasted(&self, propagations: HashMap<TxHash<Self>, Vec<String>>);

	/// Returns transaction hash
	fn hash_of(&self, xt: &TransactionFor<Self>) -> TxHash<Self>;

	/// Perform maintaince
	fn maintain(&self, block: &BlockId<Self::Block>, retracted: &[BlockHash<Self>]);
}

/// An abstraction for transaction pool.
///
/// This trait is used by offchain calls to be able to submit transactions.
/// The main use case is for offchain workers, to feed back the results of computations,
/// but since the transaction pool access is a separate `ExternalitiesExtension` it can
/// be also used in context of other offchain calls. For one may generate and submit
/// a transaction for some misbehavior reports (say equivocation).
pub trait OffchainSubmitTransaction<Block: BlockT>: Send + Sync {
	/// Submit transaction.
	///
	/// The transaction will end up in the pool and be propagated to others.
	fn submit_at(
		&self,
		at: &BlockId<Block>,
		extrinsic: Block::Extrinsic,
	) -> Result<(), ()>;
}

impl<TPool: TransactionPool> OffchainSubmitTransaction<TPool::Block> for TPool {
	fn submit_at(
		&self,
		at: &BlockId<TPool::Block>,
		extrinsic: <TPool::Block as BlockT>::Extrinsic,
	) -> Result<(), ()> {
		log::debug!(
			target: "txpool",
			"(offchain call) Submitting a transaction to the pool: {:?}",
			extrinsic
		);

		let result = futures::executor::block_on(self.submit_one(&at, extrinsic));

		result.map(|_| ())
			.map_err(|e| log::warn!(
				target: "txpool",
				"(offchain call) Error submitting a transaction to the pool: {:?}",
				e
			))
	}
}