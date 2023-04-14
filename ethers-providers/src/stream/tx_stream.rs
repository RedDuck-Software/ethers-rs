use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::{stream::Stream, Future};
use futures_util::{
    self,
    stream::{FuturesUnordered, StreamExt},
    FutureExt,
};

use ethers_core::types::{Transaction, TxHash};
use ethers_core::rand;
use tokio::time::Instant;

use crate::{
    FilterWatcher, JsonRpcClient, Middleware, Provider, ProviderError, PubsubClient,
    SubscriptionStream,
};

/// Errors `TransactionStream` can throw
#[derive(Debug, thiserror::Error)]
pub enum GetTransactionError {
    #[error("Failed to get transaction `{0}`: {1}")]
    ProviderError(TxHash, ProviderError),
    /// `get_transaction` resulted in a `None`
    #[error("Transaction `{0}` not found")]
    NotFound(TxHash),
}

impl From<GetTransactionError> for ProviderError {
    fn from(err: GetTransactionError) -> Self {
        match err {
            GetTransactionError::ProviderError(_, err) => err,
            err @ GetTransactionError::NotFound(_) => ProviderError::CustomError(err.to_string()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) type TransactionFut<'a> = Pin<Box<dyn Future<Output = TransactionResult> + Send + 'a>>;

#[cfg(target_arch = "wasm32")]
pub(crate) type TransactionFut<'a> = Pin<Box<dyn Future<Output = TransactionResult> + 'a>>;

pub(crate) type TransactionResult = Result<Transaction, GetTransactionError>;

pub(crate) struct TxHashEntry {
    hash: TxHash,
    when_seen: Instant,
}

/// Drains a stream of transaction hashes and yields entire `Transaction`.
#[must_use = "streams do nothing unless polled"]
pub struct TransactionStream<'a, P, St> {
    /// Currently running futures pending completion.
    pub(crate) pending: FuturesUnordered<TransactionFut<'a>>,
    /// Temporary buffered transaction that get started as soon as another future finishes.
    pub(crate) buffered: VecDeque<TxHashEntry>,
    /// The provider that gets the transaction
    pub(crate) provider: &'a Provider<P>,
    /// A stream of transaction hashes.
    pub(crate) stream: St,
    /// Marks if the stream is done
    stream_done: bool,
    /// max allowed futures to execute at once.
    pub(crate) max_concurrent: usize,
    /// retrieve only every n-th transaction, drop the rest
    scan_tx_fraction: u32,
    /// drop txs from buffer if they are older than this ms
    drop_after_ms: u32,
}

impl<'a, P: JsonRpcClient, St> TransactionStream<'a, P, St> {
    /// Create a new `TransactionStream` instance
    pub fn new(provider: &'a Provider<P>, stream: St, max_concurrent: usize) -> Self {
        Self::new_with_params(provider, stream, max_concurrent, 1000, u32::MAX)
    }

    /// Create a new `TransactionStream` instance
    pub fn new_with_params(provider: &'a Provider<P>, stream: St, max_concurrent: usize, scan_tx_fraction: i32, drop_after_ms: u32) -> Self {
        assert!(scan_tx_fraction > 0, "scan_tx_fraction must be > 0");
        assert!(scan_tx_fraction <= 1000, "scan_tx_fraction must be <= 1000");
        
        const SCAN_TX_FRACTION_STEP: u32 = 4294967; // 2^32 / 1000
        let scan_tx_fraction = if scan_tx_fraction == 1000 {
            u32::MAX
        } else {
            SCAN_TX_FRACTION_STEP * scan_tx_fraction as u32
        };


        Self {
            pending: Default::default(),
            buffered: Default::default(),
            provider,
            stream,
            stream_done: false,
            max_concurrent,
            scan_tx_fraction,
            drop_after_ms,
        }
    }

    /// Push a future into the set
    pub(crate) fn push_tx(&mut self, tx: TxHash) {
        {
            let hash_ptr = tx.0.as_ptr();
            let hash_ptr_u32 = hash_ptr as * const u32;
            
            #[allow(unsafe_code)]
            let first_4_bytes_of_hash_as_u32 = unsafe { *hash_ptr_u32 };
            
            // treat first 4 bytes as a random number
            if first_4_bytes_of_hash_as_u32 < self.scan_tx_fraction {
                return;
            }
        }

        let fut = self.provider.get_transaction(tx).then(move |res| match res {
            Ok(Some(tx)) => futures_util::future::ok(tx),
            Ok(None) => futures_util::future::err(GetTransactionError::NotFound(tx)),
            Err(err) => futures_util::future::err(GetTransactionError::ProviderError(tx, err)),
        });
        self.pending.push(Box::pin(fut));
    }
}

impl<'a, P, St> Stream for TransactionStream<'a, P, St>
where
    P: JsonRpcClient,
    St: Stream<Item = TxHash> + Unpin + 'a,
{
    type Item = TransactionResult;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // drain buffered transactions first
        while this.pending.len() < this.max_concurrent {
            if let Some(tx) = this.buffered.pop_front() {
                let millis = tx.when_seen.elapsed().as_millis() as u32;
                if millis > this.drop_after_ms {
                    continue;
                }

                this.push_tx(tx.hash);
            } else {
                break;
            }
        }

        if !this.stream_done {
            loop {
                match Stream::poll_next(Pin::new(&mut this.stream), cx) {
                    Poll::Ready(Some(tx)) => {
                        if this.pending.len() < this.max_concurrent {
                            this.push_tx(tx);
                        } else {
                            let entry = TxHashEntry { hash: tx, when_seen: Instant::now() };
                            this.buffered.push_back(entry);
                        }
                    }
                    Poll::Ready(None) => {
                        this.stream_done = true;
                        break;
                    }
                    _ => break,
                }
            }
        }

        // poll running futures
        if let tx @ Poll::Ready(Some(_)) = this.pending.poll_next_unpin(cx) {
            return tx;
        }

        if this.stream_done && this.pending.is_empty() {
            // all done
            return Poll::Ready(None);
        }

        Poll::Pending
    }
}

impl<'a, P> FilterWatcher<'a, P, TxHash>
where
    P: JsonRpcClient,
{
    /// Returns a stream that yields the `Transaction`s for the transaction hashes this stream
    /// yields.
    ///
    /// This internally calls `Provider::get_transaction` with every new transaction.
    /// No more than n futures will be buffered at any point in time, and less than n may also be
    /// buffered depending on the state of each future.
    pub fn transactions_unordered(self, n: usize) -> TransactionStream<'a, P, Self> {
        TransactionStream::new(self.provider, self, n)
    }

    /// Returns a stream that yields the `Transaction`s for the transaction hashes this stream
    /// yields.
    ///
    /// This internally calls `Provider::get_transaction` with every new transaction.
    /// No more than n futures will be buffered at any point in time, and less than n may also be
    /// buffered depending on the state of each future.
    pub fn transactions_unordered_override(self, n: usize, scan_tx_fraction: i32, drop_after_ms: u32) -> TransactionStream<'a, P, Self> {
        TransactionStream::new_with_params(self.provider, self, n, scan_tx_fraction, drop_after_ms)
    }
}

impl<'a, P> SubscriptionStream<'a, P, TxHash>
where
    P: PubsubClient,
{
    /// Returns a stream that yields the `Transaction`s for the transaction hashes this stream
    /// yields.
    ///
    /// This internally calls `Provider::get_transaction` with every new transaction.
    /// No more than n futures will be buffered at any point in time, and less than n may also be
    /// buffered depending on the state of each future.
    pub fn transactions_unordered(self, n: usize) -> TransactionStream<'a, P, Self> {
        TransactionStream::new(self.provider, self, n)
    }

    /// Returns a stream that yields the `Transaction`s for the transaction hashes this stream
    /// yields.
    ///
    /// This internally calls `Provider::get_transaction` with every new transaction.
    /// No more than n futures will be buffered at any point in time, and less than n may also be
    /// buffered depending on the state of each future.
    pub fn transactions_unordered_override(self, n: usize, scan_tx_fraction: i32, drop_after_ms: u32) -> TransactionStream<'a, P, Self> {
        TransactionStream::new_with_params(self.provider, self, n, scan_tx_fraction, drop_after_ms)
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::{stream::tx_stream, Http, Ws};
    use ethers_core::{
        types::{Transaction, TransactionReceipt, TransactionRequest},
        utils::Anvil,
    };
    use futures_util::{FutureExt, StreamExt};
    use std::{collections::HashSet, time::Duration};

    #[tokio::test]
    async fn can_stream_pending_transactions() {
        let num_txs = 5;
        let geth = Anvil::new().block_time(2u64).spawn();
        let provider = Provider::<Http>::try_from(geth.endpoint())
            .unwrap()
            .interval(Duration::from_millis(1000));
        let ws = Ws::connect(geth.ws_endpoint()).await.unwrap();
        let ws_provider = Provider::new(ws);

        let accounts = provider.get_accounts().await.unwrap();
        let tx = TransactionRequest::new().from(accounts[0]).to(accounts[0]).value(1e18 as u64);

        let mut sending = futures_util::future::join_all(
            std::iter::repeat(tx.clone())
                .take(num_txs)
                .enumerate()
                .map(|(nonce, tx)| tx.nonce(nonce))
                .map(|tx| async {
                    provider.send_transaction(tx, None).await.unwrap().await.unwrap().unwrap()
                }),
        )
        .fuse();

        let mut watch_tx_stream = provider
            .watch_pending_transactions()
            .await
            .unwrap()
            .transactions_unordered(num_txs)
            .fuse();

        let mut sub_tx_stream =
            ws_provider.subscribe_pending_txs().await.unwrap().transactions_unordered(2).fuse();

        let mut sent: Option<Vec<TransactionReceipt>> = None;
        let mut watch_received: Vec<Transaction> = Vec::with_capacity(num_txs);
        let mut sub_received: Vec<Transaction> = Vec::with_capacity(num_txs);

        loop {
            futures_util::select! {
                txs = sending => {
                    sent = Some(txs)
                },
                tx = watch_tx_stream.next() => watch_received.push(tx.unwrap().unwrap()),
                tx = sub_tx_stream.next() => sub_received.push(tx.unwrap().unwrap()),
            };
            if watch_received.len() == num_txs && sub_received.len() == num_txs {
                if let Some(ref sent) = sent {
                    assert_eq!(sent.len(), watch_received.len());
                    let sent_txs =
                        sent.iter().map(|tx| tx.transaction_hash).collect::<HashSet<_>>();
                    assert_eq!(sent_txs, watch_received.iter().map(|tx| tx.hash).collect());
                    assert_eq!(sent_txs, sub_received.iter().map(|tx| tx.hash).collect());
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn can_stream_transactions() {
        let anvil = Anvil::new().block_time(2u64).spawn();
        let provider =
            Provider::<Http>::try_from(anvil.endpoint()).unwrap().with_sender(anvil.addresses()[0]);

        let accounts = provider.get_accounts().await.unwrap();

        let tx = TransactionRequest::new().from(accounts[0]).to(accounts[0]).value(1e18 as u64);
        let txs = vec![tx.clone().nonce(0u64), tx.clone().nonce(1u64), tx.clone().nonce(2u64)];

        let txs =
            futures_util::future::join_all(txs.into_iter().map(|tx| async {
                provider.send_transaction(tx, None).await.unwrap().await.unwrap()
            }))
            .await;

        let stream = tx_stream::TransactionStream::new(
            &provider,
            futures_util::stream::iter(txs.iter().cloned().map(|tx| tx.unwrap().transaction_hash)),
            10,
        );
        let res =
            stream.collect::<Vec<_>>().await.into_iter().collect::<Result<Vec<_>, _>>().unwrap();

        assert_eq!(res.len(), txs.len());
        assert_eq!(
            res.into_iter().map(|tx| tx.hash).collect::<HashSet<_>>(),
            txs.into_iter().map(|tx| tx.unwrap().transaction_hash).collect()
        );
    }
}
