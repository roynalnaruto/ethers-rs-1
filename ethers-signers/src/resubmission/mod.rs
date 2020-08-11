use ethers_core::types::{TransactionReceipt, TransactionRequest, TxHash, U256};
use ethers_providers::{
    interval, JsonRpcClient, PendingTransaction, ProviderError, DEFAULT_POLL_INTERVAL,
};
use futures_core::stream::Stream;
use futures_util::{ready, stream::StreamExt};
use pin_project::pin_project;

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use crate::{Client, ClientError, Signer};

#[pin_project]
pub struct Resubmission<'a, P, S> {
    client: &'a Client<P, S>,
    interval: Box<dyn Stream<Item = ()> + Send + Unpin>,
    transaction: TransactionRequest,
    tx_hashes: Vec<TxHash>,
    confirmations: usize,
    state: ResubmissionState<'a, P>,
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

        let pending_tx_fut = Box::pin(
            PendingTransaction::new(tx_hash.clone(), client.provider())
                .confirmations(confirmations)
                .interval(client.provider().get_interval()),
        );

        Self {
            client,
            interval: Box::new(interval(DEFAULT_POLL_INTERVAL)),
            transaction: tx,
            tx_hashes: vec![tx_hash],
            confirmations,
            state: ResubmissionState::GettingReceipt(vec![pending_tx_fut]),
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

        match this.state {
            ResubmissionState::GettingReceipt(pending_tx_futs) => {
                let _ready = ready!(this.interval.poll_next_unpin(ctx));

                // to mark whether any one of the futures has been completed
                let mut completed: Option<TransactionReceipt> = None;

                // iterate over all the pending transactions to check if any one is ready
                for (i, fut) in pending_tx_futs.into_iter().enumerate() {
                    println!("fut {}", i);
                    if let Poll::Ready(Ok(receipt)) = fut.as_mut().poll(ctx) {
                        println!("found_one");
                        completed = Some(receipt);
                        break;
                    }
                }

                match completed {
                    // if one of the pending transactions was ready
                    // return the receipt and transition to Completed
                    Some(receipt) => {
                        *this.state = ResubmissionState::Completed;
                        return Poll::Ready(Ok(receipt));
                    }
                    // if none of the pending transactions was ready
                    // build another transaction by modifying the gas price
                    // transition to Resubmitting with the new transaction
                    None => {
                        let mut resubmit_tx = this.transaction.clone();
                        resubmit_tx.gas_price = Some(U256::from(3123123123u128));
                        let resubmit_tx_fut =
                            Box::pin(this.client.send_transaction(resubmit_tx.clone(), None));

                        *this.transaction = resubmit_tx;
                        *this.state = ResubmissionState::Resubmitting(resubmit_tx_fut);
                    }
                }
            }
            ResubmissionState::Resubmitting(resubmit_tx_fut) => {
                let _ready = ready!(this.interval.poll_next_unpin(ctx));

                // poll until we have the transaction hash of this newly broadcasted transaction
                // take the tx_hash and transition to the GettingReceipt state
                // push another pending transaction future to the existing ones
                if let Ok(tx_hash) = ready!(resubmit_tx_fut.as_mut().poll(ctx)) {
                    this.tx_hashes.push(tx_hash);
                    let client = this.client.clone();
                    let confs = *this.confirmations;
                    let pending_tx_futs = this
                        .tx_hashes
                        .iter()
                        .map(|tx| {
                            Box::pin(
                                PendingTransaction::new(tx.clone(), client.provider())
                                    .confirmations(confs)
                                    .interval(client.provider().get_interval()),
                            )
                        })
                        .collect();
                    *this.state = ResubmissionState::GettingReceipt(pending_tx_futs);
                } else {
                    let resubmit_tx_fut =
                        Box::pin(this.client.send_transaction(this.transaction.clone(), None));
                    *this.state = ResubmissionState::Resubmitting(resubmit_tx_fut);
                }
            }
            ResubmissionState::Completed => {
                panic!("polled tx resubmission future after completion")
            }
        }

        Poll::Pending
    }
}

// Helper type aliases
type PendingTxFut<'a, P> = Pin<Box<PendingTransaction<'a, P>>>;
type SendTxFut<'a> = Pin<Box<dyn Future<Output = Result<TxHash, ClientError>> + 'a + Send>>;

enum ResubmissionState<'a, P> {
    /// Polling the blockchain for transaction receipt. This holds a list of pending transaction
    /// futures since we might have submitted the transaction more than once.
    /// If any one of the pending transaction futures resolves, transition to the Completed
    /// state while returning the resolved receipt.
    /// If none of the pending transaction futures resolves, resubmit the transaction based on
    /// the resubmission policy and transition to the Resubmitting state.
    GettingReceipt(Vec<PendingTxFut<'a, P>>),

    /// Resubmitting the transaction based on the resubmission policy.
    /// When the resubmitted transaction resolves to its transaction hash, we transition
    /// back to the GettingReceipt state after pushing the new pending transaction future
    /// to the list of pending transaction futures.
    Resubmitting(SendTxFut<'a>),

    /// One of the broadcasted transactions has been resolved to a receipt, and the future
    /// has completed. Further polling it should panic.
    Completed,
}
