use ethers_core::types::{TransactionReceipt, TransactionRequest, TxHash, U256};
use ethers_providers::{
    interval, JsonRpcClient, PendingTransaction, ProviderError, DEFAULT_POLL_INTERVAL,
};
use futures::executor::block_on;
use futures_core::stream::Stream;
use futures_util::stream::StreamExt;
use pin_project::pin_project;

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use crate::{Client, Signer};

type PinBoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProviderError>> + 'a + Send>>;

#[pin_project]
pub struct Resubmission<'a, P, S> {
    client: &'a Client<P, S>,
    interval: Box<dyn Stream<Item = ()> + Send + Unpin>,
    transaction: TransactionRequest,
    confirmations: usize,
    pending_txs: Vec<PinBoxFut<'a, TransactionReceipt>>,
}

impl<'a, P, S> Resubmission<'a, P, S>
where
    P: JsonRpcClient,
    S: Signer,
{
    /// Creates a new `Resubmission` instance, which is capable of polling
    /// the blockchain for the status of the given transaction and resubmitting
    /// it by bumping up the gas price based on the chosen resubmission policy
    pub fn new(
        client: &'a Client<P, S>,
        tx: TransactionRequest,
        tx_hash: TxHash,
        confirmations: Option<usize>,
    ) -> Self {
        let confirmations = confirmations.unwrap_or(1usize);

        let pending_tx = PendingTransaction::new(tx_hash, client.provider())
            .confirmations(confirmations)
            .interval(client.provider().get_interval());

        Self {
            client,
            interval: Box::new(interval(DEFAULT_POLL_INTERVAL)),
            transaction: tx,
            confirmations,
            pending_txs: vec![Box::pin(pending_tx)],
        }
    }

    /// Sets the polling interval
    pub fn interval<T: Into<Duration>>(mut self, duration: T) -> Self {
        self.interval = Box::new(interval(duration.into()));
        self
    }
}

impl<'a, P, S> Future for Resubmission<'a, P, S>
where
    P: JsonRpcClient,
    S: Signer,
{
    type Output = Result<TransactionReceipt, ProviderError>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();

        let _ready = futures_util::ready!(this.interval.poll_next_unpin(ctx));

        // initialise to the state where none of the pending transactions have resolved
        let mut completed: Option<TransactionReceipt> = None;

        // if any of the pending transactions have resolved, break out
        for pending_tx in this.pending_txs.into_iter() {
            println!("query");
            if let Poll::Ready(Ok(receipt)) = pending_tx.as_mut().poll(ctx) {
                println!("found");
                completed = Some(receipt);
                break;
            }
        }

        if let Some(receipt) = completed {
            return Poll::Ready(Ok(receipt));
        } else {
            // TODO: move this to a resubmission policy, dummy for now
            let mut tx = this.transaction.clone();
            if let Some(gas_price) = tx.gas_price {
                tx.gas_price = Some(U256::from(2u128 * gas_price.as_u128()));
            } else {
                tx.gas_price = Some(U256::from(3123123123u128));
            }
            // TODO: move this to a resubmission policy, dummy for now

            let resubmit_tx_fut = this.client.send_transaction(tx, None);
            let resubmit_tx_hash = block_on(async {
                println!("broadcast here");
                match resubmit_tx_fut.await {
                    Ok(tx_hash) => tx_hash,
                    Err(e) => panic!("Error re-submitting transaction: {}", e),
                }
            });
            println!("never arrive here");

            let pending_tx_fut = Box::pin(
                PendingTransaction::new(resubmit_tx_hash, this.client.provider())
                    .confirmations(*this.confirmations),
            );
            this.pending_txs.push(pending_tx_fut);
        }

        Poll::Pending
    }
}
