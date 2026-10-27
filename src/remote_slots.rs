//! Synchronous bounded admission before a remote request body is read.
use anyhow::{Result, bail, ensure};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
struct Waiter {
    id: Uuid,
    sender: oneshot::Sender<RunSlot>,
    cancel: CancellationToken,
}
struct State {
    active: usize,
    waiting: VecDeque<Waiter>,
}
struct Inner {
    capacity: usize,
    queue_capacity: usize,
    state: Mutex<State>,
}
#[derive(Clone)]
pub struct RunSlots {
    inner: Arc<Inner>,
}
pub struct RunSlot {
    inner: Option<Arc<Inner>>,
}
pub struct Reservation {
    inner: Arc<Inner>,
    id: Option<Uuid>,
    ready: Option<RunSlot>,
    receiver: Option<oneshot::Receiver<RunSlot>>,
    cancel: CancellationToken,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub active: usize,
    pub queued: usize,
    pub capacity: usize,
    pub queue_capacity: usize,
    pub saturated: bool,
    pub position: usize,
}
impl RunSlots {
    pub fn new(capacity: usize, queue_capacity: usize) -> Result<Self> {
        ensure!(
            capacity > 0
                && capacity as u128 <= 9_007_199_254_740_991
                && queue_capacity as u128 <= 9_007_199_254_740_991,
            "Run capacity must be a positive safe integer and queue capacity a nonnegative safe integer"
        );
        Ok(Self {
            inner: Arc::new(Inner {
                capacity,
                queue_capacity,
                state: Mutex::new(State {
                    active: 0,
                    waiting: VecDeque::new(),
                }),
            }),
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .waiting
            .retain(|waiter| !waiter.cancel.is_cancelled() && !waiter.sender.is_closed());
        Snapshot {
            active: state.active,
            queued: state.waiting.len(),
            capacity: self.inner.capacity,
            queue_capacity: self.inner.queue_capacity,
            saturated: state.active >= self.inner.capacity
                && state.waiting.len() >= self.inner.queue_capacity,
            position: if state.active < self.inner.capacity {
                0
            } else {
                state.waiting.len() + 1
            },
        }
    }
    /// Enqueue now, before the caller awaits admission or buffers a request body.
    pub fn reserve(&self, cancel: &CancellationToken) -> Result<Reservation> {
        ensure!(
            !cancel.is_cancelled(),
            "cancelled before a slot was available"
        );
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .waiting
            .retain(|waiter| !waiter.cancel.is_cancelled() && !waiter.sender.is_closed());
        let mut reservation = Reservation {
            inner: self.inner.clone(),
            id: None,
            ready: None,
            receiver: None,
            cancel: cancel.clone(),
        };
        if state.active < self.inner.capacity {
            state.active += 1;
            reservation.ready = Some(RunSlot {
                inner: Some(self.inner.clone()),
            });
        } else {
            ensure!(
                state.waiting.len() < self.inner.queue_capacity,
                "The browser run queue is full."
            );
            let id = Uuid::new_v4();
            let (sender, receiver) = oneshot::channel();
            state.waiting.push_back(Waiter {
                id,
                sender,
                cancel: cancel.clone(),
            });
            reservation.id = Some(id);
            reservation.receiver = Some(receiver);
        }
        Ok(reservation)
    }
}
impl Reservation {
    /// Still queued: no slot has been granted yet.
    pub fn waiting(&self) -> bool {
        self.id.is_some_and(|id| {
            self.inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .waiting
                .iter()
                .any(|waiter| waiter.id == id)
        })
    }
    pub async fn wait(mut self) -> Result<RunSlot> {
        if let Some(slot) = self.ready.take() {
            ensure!(
                !self.cancel.is_cancelled(),
                "cancelled before a slot was available"
            );
            return Ok(slot);
        }
        let receiver = self
            .receiver
            .take()
            .expect("Queued reservation has its receiver");
        tokio::select! {biased;
            _=self.cancel.cancelled()=>bail!("cancelled while waiting for a slot"),
            slot=receiver=>Ok(slot.map_err(|_|anyhow::anyhow!("Run admission owner ended before a slot was available"))?),
        }
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(index) = state.waiting.iter().position(|waiter| waiter.id == id) {
                state.waiting.remove(index);
            }
        }
        // A granted receiver returns its slot after the queue lock is released.
    }
}
impl RunSlot {
    pub fn release(mut self) {
        if let Some(inner) = self.inner.take() {
            release(&inner);
        }
    }
}
impl Drop for RunSlot {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            release(&inner);
        }
    }
}
fn release(inner: &Arc<Inner>) {
    loop {
        let sender = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let next = loop {
                match state.waiting.pop_front() {
                    Some(waiter) if !waiter.sender.is_closed() && !waiter.cancel.is_cancelled() => {
                        break Some(waiter.sender);
                    }
                    Some(_) => continue,
                    None => break None,
                }
            };
            if next.is_none() {
                state.active = state
                    .active
                    .checked_sub(1)
                    .expect("Only an active slot releases admission");
            }
            next
        };
        let Some(sender) = sender else {
            return;
        };
        match sender.send(RunSlot {
            inner: Some(inner.clone()),
        }) {
            Ok(()) => return,
            Err(mut unused) => {
                unused.inner.take();
            } // A closed receiver is skipped without recursive release.
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reservation_is_synchronous_bounded_and_fifo() {
        let slots = RunSlots::new(1, 2).unwrap();
        let cancel = CancellationToken::new();
        let first = slots.reserve(&cancel).unwrap().wait().await.unwrap();
        let second = slots.reserve(&cancel).unwrap();
        let third = slots.reserve(&cancel).unwrap();
        assert_eq!(
            slots.snapshot(),
            Snapshot {
                active: 1,
                queued: 2,
                capacity: 1,
                queue_capacity: 2,
                saturated: true,
                position: 3
            }
        );
        assert!(slots.reserve(&cancel).is_err());
        first.release();
        let second = second.wait().await.unwrap();
        assert_eq!(slots.snapshot().queued, 1);
        second.release();
        let third = third.wait().await.unwrap();
        third.release();
        assert_eq!(slots.snapshot().active, 0);
        assert_eq!(slots.snapshot().position, 0);
    }
    #[tokio::test]
    async fn cancelled_and_dropped_waiters_free_queue_capacity_without_reordering() {
        let slots = RunSlots::new(1, 2).unwrap();
        let cancel = CancellationToken::new();
        let first = slots.reserve(&cancel).unwrap().wait().await.unwrap();
        let cancelled = CancellationToken::new();
        let second = slots.reserve(&cancelled).unwrap();
        let third = slots.reserve(&cancel).unwrap();
        cancelled.cancel();
        assert!(second.wait().await.is_err());
        assert_eq!(slots.snapshot().queued, 1);
        let fourth = slots.reserve(&cancel).unwrap();
        drop(fourth);
        assert_eq!(slots.snapshot().queued, 1);
        drop(first);
        let third = third.wait().await.unwrap();
        drop(third);
        assert_eq!(slots.snapshot().active, 0);
    }
    #[tokio::test]
    async fn dropping_a_granted_unawaited_ticket_returns_capacity_to_the_next_waiter() {
        let slots = RunSlots::new(1, 2).unwrap();
        let cancel = CancellationToken::new();
        let first = slots.reserve(&cancel).unwrap().wait().await.unwrap();
        let second = slots.reserve(&cancel).unwrap();
        let third = slots.reserve(&cancel).unwrap();
        drop(first);
        drop(second);
        let third = third.wait().await.unwrap();
        assert_eq!(slots.snapshot().active, 1);
        drop(third);
        assert_eq!(slots.snapshot().active, 0);
    }
    #[tokio::test]
    async fn cancelled_unpolled_ticket_does_not_block_later_reservation() {
        let slots = RunSlots::new(1, 1).unwrap();
        let cancel = CancellationToken::new();
        let first = slots.reserve(&cancel).unwrap().wait().await.unwrap();
        let cancelled = CancellationToken::new();
        let second = slots.reserve(&cancelled).unwrap();
        cancelled.cancel();
        assert_eq!(slots.snapshot().queued, 0);
        let third = slots.reserve(&cancel).unwrap();
        drop(first);
        assert!(second.wait().await.is_err());
        let third = third.wait().await.unwrap();
        drop(third);
        assert_eq!(slots.snapshot().active, 0);
    }
    #[tokio::test]
    async fn pre_cancelled_requests_and_zero_queue_capacity_cannot_reserve() {
        assert!(RunSlots::new(0, 0).is_err());
        let slots = RunSlots::new(1, 0).unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(slots.reserve(&cancelled).is_err());
        assert_eq!(slots.snapshot().active, 0);
        let reservation = slots.reserve(&CancellationToken::new()).unwrap();
        assert!(slots.snapshot().saturated);
        assert!(slots.reserve(&CancellationToken::new()).is_err());
        drop(reservation);
        assert_eq!(slots.snapshot().active, 0);
    }
}
