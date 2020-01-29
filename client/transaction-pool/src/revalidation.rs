use std::{sync::Arc, collections::{HashMap, HashSet, BTreeMap}};
use sc_transaction_graph::{ChainApi, Pool, ExHash, NumberFor};
use sp_runtime::traits::{
	Block as BlockT, Header as HeaderT, SimpleArithmetic, One, Zero,
};
use sp_runtime::generic::BlockId;
use sp_runtime::transaction_validity::{InvalidTransaction, TransactionValidityError};

use futures::{prelude::*, channel::mpsc, future, task::Poll, stream::unfold};
use std::pin::Pin;
use std::time::Duration;
use futures_timer::Delay;

const BACKGROUND_REVALIDATION_INTERVAL: Duration = Duration::from_millis(200);

struct WorkerPayload<Api: ChainApi> {
	at: NumberFor<Api>,
	transactions: Vec<ExHash<Api>>,
}

struct RevalidationWorker<Api: ChainApi> {
	api: Arc<Api>,
	pool: Arc<Pool<Api>>,
	best_block: NumberFor<Api>,
	from_queue: mpsc::UnboundedReceiver<WorkerPayload<Api>>,
	tick_timeout: Pin<Box<dyn Stream<Item = ()> + Send>>,
	block_ordered: BTreeMap<NumberFor<Api>, HashSet<ExHash<Api>>>,
	members: HashMap<ExHash<Api>, NumberFor<Api>>,
}

impl<Api: ChainApi> Unpin for RevalidationWorker<Api> {}

fn interval(duration: Duration) -> impl Stream<Item=()> + Unpin {
	unfold((), move |_| {
		Delay::new(duration).map(|_| Some(((), ())))
	}).map(drop)
}

async fn batch_revalidate<Api: ChainApi>(
	pool: Arc<Pool<Api>>,
	api: Arc<Api>,
	at: NumberFor<Api>,
	batch: impl IntoIterator<Item=ExHash<Api>>,
) {
	let mut invalid_hashes = Vec::new();

	for ext_hash in batch.into_iter() {
		let ext = match pool.ready_transaction(&ext_hash) {
			Some(ext) => ext,
			None => continue,
		};

		match api.validate_transaction(&BlockId::Number(at), ext.data.clone()).await {
			Ok(Err(TransactionValidityError::Invalid(_))) => invalid_hashes.push(ext_hash),
			_ => continue,
		}
	}

	pool.remove_invalid(&invalid_hashes);
}

impl<Api: ChainApi> RevalidationWorker<Api> {
	fn new(
		api: Arc<Api>,
		pool: Arc<Pool<Api>>,
		from_queue: mpsc::UnboundedReceiver<WorkerPayload<Api>>
	) -> Self {
		Self {
			api,
			pool,
			from_queue,
			tick_timeout: Box::pin(interval(BACKGROUND_REVALIDATION_INTERVAL)),
			block_ordered: Default::default(),
			members: Default::default(),
			best_block: Zero::zero(),
		}
	}

	fn prepare_batch(&mut self) -> Vec<ExHash<Api>> {
		let count = 20;
		let mut queued_exts = Vec::new();
		let mut empty = Vec::<NumberFor<Api>>::new();

		// Take maximum of count transaction by order
		// which they got into the pool
		for (block_number, mut ext_map) in self.block_ordered.iter_mut() {
			if queued_exts.len() >= count { break; }

			loop {
				let next_key = match ext_map.iter().nth(0) {
					Some(k) => k.clone(),
					None => { break; }
				};

				ext_map.remove(&next_key);
				self.members.remove(&next_key);

				queued_exts.push(next_key);

				if ext_map.len() == 0 { empty.push(*block_number); }

				if queued_exts.len() >= count { break; }
			}
		}

		// retain only non-empty
		for empty_block_number in empty.into_iter() { self.block_ordered.remove(&empty_block_number); }

		queued_exts
	}

	fn push(&mut self, worker_payload: WorkerPayload<Api>) {
		// we don't add something that already scheduled for revalidation
		let transactions = worker_payload.transactions;
		let block_number = worker_payload.at;

		for ext_hash in transactions.into_iter() {

			// we don't add something that already scheduled for revalidation
			if self.members.contains_key(&ext_hash) { continue; }

			self.block_ordered.entry(block_number)
				.and_modify(|value| { value.insert(ext_hash.clone()); })
				.or_insert_with(|| {
					let mut bt = HashSet::new();
					bt.insert(ext_hash.clone());
					bt
				});
			self.members.insert(ext_hash.clone(), block_number);
		}
	}
}

impl<Api: ChainApi> Future for RevalidationWorker<Api>
where Api: 'static,
{
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
		let this = &mut *self;
		let mut process_some_immediately = false;

		while let Poll::Ready(Some(worker_payload)) = this.from_queue.poll_next_unpin(cx) {
			this.best_block = worker_payload.at;
			this.push(worker_payload);
			process_some_immediately = true;
		}

		loop {
			match (this.tick_timeout.poll_next_unpin(cx), process_some_immediately) {
				(Poll::Ready(Some(_)), _) | (_, true) => {
					process_some_immediately = false;
					let next_batch = this.prepare_batch();
					let batch_revalidate_fut =
						batch_revalidate(this.pool.clone(), this.api.clone(), this.best_block, next_batch);
					futures::pin_mut!(batch_revalidate_fut);
					match batch_revalidate_fut.poll_unpin(cx)
					{
						Poll::Ready(_) => continue,
						_ => break,
					}
				},
				_ => break,
			}
		}


		Poll::Pending
	}
}

struct BackgroundConfig<Api: ChainApi> {
	to_worker: mpsc::UnboundedSender<WorkerPayload<Api>>,
	spawner: futures::executor::ThreadPool,
}

pub struct RevalidationQueue<Api: ChainApi> {
	pool: Arc<Pool<Api>>,
	api: Arc<Api>,
	background: Option<BackgroundConfig<Api>>,
}

impl<Api: ChainApi> RevalidationQueue<Api>
where
	Api: 'static,
{
	pub fn new_sync(api: Arc<Api>, pool: Arc<Pool<Api>>) -> Self {
		Self {
			api,
			pool,
			background: None,
		}
	}

	pub fn new_background(api: Arc<Api>, pool: Arc<Pool<Api>>) -> Self {
		let spawner = futures::executor::ThreadPool::builder()
			.name_prefix("txpool-worker")
			.pool_size(1)
			.create()
			.expect("Creating worker thread task failed");

		let (to_worker, from_queue) = mpsc::unbounded();

		let worker = RevalidationWorker::new(api.clone(), pool.clone(), from_queue);
		spawner.spawn_ok(worker);

		Self {
			api,
			pool,
			background: Some(
				BackgroundConfig { spawner, to_worker }
			),
		}
	}

	pub async fn offload(&self, at: NumberFor<Api>, transactions: Vec<ExHash<Api>>) {
		if let Some(ref background) = self.background {
			background.to_worker.unbounded_send(WorkerPayload { at, transactions })
				.expect("background task is never dropped");
			return;
		} else {
			let pool = self.pool.clone();
			let api = self.api.clone();
			batch_revalidate(pool, api, at, transactions).await
		}
	}
}

#[cfg(test)]
mod tests {

	use super::*;
	use sc_transaction_graph::{ChainApi, Pool, ExHash};
	use crate::testing::api::TestApi;

	fn setup() -> (Arc<TestApi>, Pool<TestApi>) {
		let test_api = Arc::new(TestApi::empty());
		let pool = Pool::new(Default::default(), test_api.clone());
		(test_api, pool)
	}

	#[test]
	fn revalidation_can_bet_instantiated() {
		let (api, pool) = setup();

		let queue = RevalidationQueue::new_sync(api, Arc::new(pool));
	}

}