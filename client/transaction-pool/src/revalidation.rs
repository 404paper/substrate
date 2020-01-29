use std::sync::Arc;
use sc_transaction_graph::{ChainApi, Pool, ExHash};
use futures::{prelude::*, channel::mpsc, future, task::Poll, stream::unfold};
use std::pin::Pin;
use std::time::Duration;
use futures_timer::Delay;

const REVALIDATION_INTERVAL: Duration = Duration::from_millis(200);

type WorkerPayload<A> = Vec<ExHash<A>>;

struct RevalidationWorker<Api: ChainApi> {
	api: Arc<Api>,
	pool: Arc<Pool<Api>>,
	transactions: Vec<ExHash<Api>>,
	from_queue: mpsc::UnboundedReceiver<WorkerPayload<Api>>,
	tick_timeout: Pin<Box<dyn Stream<Item = ()> + Send>>,
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
			transactions: Default::default(),
			tick_timeout: Box::pin(interval(REVALIDATION_INTERVAL)),
		}
	}
}

fn interval(duration: Duration) -> impl Stream<Item=()> + Unpin {
	unfold((), move |_| {
		Delay::new(duration).map(|_| Some(((), ())))
	}).map(drop)
}

async fn batch_revalidate<Api: ChainApi>(
	pool: Arc<Pool<Api>>,
	api: Arc<Api>,
	batch: impl IntoIterator<Item=ExHash<Api>>,
) {
}

impl<Api: ChainApi> RevalidationWorker<Api> {
	fn prepare_batch(&mut self) -> Vec<ExHash<Api>> {
		self.transactions.drain(..20).collect()
	}
}

impl<Api: ChainApi> Future for RevalidationWorker<Api>
where Api: 'static,
	<Api as ChainApi>::Hash: Unpin,
{
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
		let this = Pin::into_inner(self);

		while let Poll::Ready(Some(_)) = this.tick_timeout.poll_next_unpin(cx) {
			let next_batch = this.prepare_batch();
			let batch_revalidate_fut =
				batch_revalidate(this.pool.clone(), this.api.clone(), next_batch);
			futures::pin_mut!(batch_revalidate_fut);
			match batch_revalidate_fut.poll_unpin(cx)
			{
				Poll::Ready(_) => continue,
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
	<Api as ChainApi>::Hash: Unpin,
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

	pub async fn offload(&self, transactions: Vec<ExHash<Api>>) {
		if let Some(ref background) = self.background {
			background.to_worker.unbounded_send(transactions)
				.expect("background task is never dropped");
			return;
		} else {
			let pool = self.pool.clone();
			let api = self.api.clone();
			batch_revalidate(pool, api, transactions).await
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

		let queue =
			RevalidationQueue::new_sync(api, Arc::new(pool));
	}

}