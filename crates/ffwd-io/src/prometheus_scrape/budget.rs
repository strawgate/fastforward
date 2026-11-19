//! Memory budget shared by every target of one `prometheus_scrape` input.
//!
//! A scrape holds a [`Reservation`] from before its request until the
//! pipeline takes its batch, so response bodies being read and batches
//! waiting in the channel together stay within `max_inflight_bytes`.
//!
//! Before a request a scrape reserves its estimate (what its previous scrape
//! needed) and waits while the budget is exhausted, holding nothing; that
//! wait is what delays scrapes when the output is slow. If the scrape then
//! needs more:
//!
//! - for the response body and gunzip output, it reserves before growing
//!   the buffer and may wait only until its timeout. Waiting while holding
//!   part of the budget could otherwise deadlock targets that each hold some
//!   and wait for more.
//! - an Arrow batch's size is known only once it is built, so it is covered
//!   right after, without waiting ([`Reservation::try_grow_to`]). Building
//!   is synchronous on the input's single-threaded runtime, so the budget is
//!   exceeded by at most one batch at a time, and only by a scrape that
//!   outgrew its estimate (its first, or one whose target grew).
//!
//! A scrape that can't get the extra is counted as failed and drops its
//! data; its next attempt reserves enough up front.
//!
//! A single scrape larger than the whole budget takes all of it, so it runs
//! alone instead of failing.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// Budget accounting unit, in bytes. Semaphore permits are counted in `u32`.
const UNIT: usize = 1024;

/// Smallest reservation a scrape starts with.
pub const MIN_RESERVATION_BYTES: usize = 64 * 1024;

/// Byte budget shared by the targets of one input.
pub struct Budget {
    sem: Arc<Semaphore>,
    units: u32,
}

impl Budget {
    /// A budget of `bytes` (at least one unit).
    pub fn new(bytes: usize) -> Self {
        let units = u32::try_from(bytes.div_ceil(UNIT))
            .unwrap_or(u32::MAX)
            .clamp(1, u32::try_from(Semaphore::MAX_PERMITS).unwrap_or(u32::MAX));
        Self {
            sem: Arc::new(Semaphore::new(units as usize)),
            units,
        }
    }

    /// Bytes not reserved by any scrape.
    #[cfg(test)]
    pub fn available_bytes(&self) -> usize {
        self.sem.available_permits().saturating_mul(UNIT)
    }

    fn units_for(&self, bytes: usize) -> u32 {
        u32::try_from(bytes.div_ceil(UNIT))
            .unwrap_or(u32::MAX)
            .clamp(1, self.units)
    }

    /// Reserve `bytes` (capped at the whole budget), waiting as long as it
    /// takes. Returns the reservation and whether the call had to wait.
    pub async fn reserve(
        self: &Arc<Self>,
        bytes: usize,
    ) -> Result<(Reservation, bool), OverBudget> {
        let units = self.units_for(bytes);
        let (permit, waited) = match Arc::clone(&self.sem).try_acquire_many_owned(units) {
            Ok(p) => (p, false),
            Err(_) => match Arc::clone(&self.sem).acquire_many_owned(units).await {
                Ok(p) => (p, true),
                // The semaphore is never closed; treat it like no room.
                Err(_) => return Err(OverBudget { needed: bytes }),
            },
        };
        Ok((
            Reservation {
                budget: Arc::clone(self),
                permit,
            },
            waited,
        ))
    }
}

/// Bytes of the shared budget held by one scrape. Released on drop.
pub struct Reservation {
    budget: Arc<Budget>,
    permit: OwnedSemaphorePermit,
}

/// The budget had no room for a scrape's growth before its deadline.
#[derive(Debug)]
pub struct OverBudget {
    /// Bytes the scrape needed.
    pub needed: usize,
}

impl Reservation {
    /// Bytes currently held.
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.permit.num_permits().saturating_mul(UNIT)
    }

    /// Hold at least `bytes` (capped at the whole budget). Waits for the
    /// extra until `deadline` at most.
    pub async fn grow_to(&mut self, bytes: usize, deadline: Instant) -> Result<(), OverBudget> {
        let want = self.budget.units_for(bytes);
        let held = u32::try_from(self.permit.num_permits()).unwrap_or(u32::MAX);
        if want <= held {
            return Ok(());
        }
        let extra = want - held;
        let sem = Arc::clone(&self.budget.sem);
        let more = match Arc::clone(&sem).try_acquire_many_owned(extra) {
            Ok(p) => p,
            Err(_) => {
                match tokio::time::timeout_at(deadline, sem.acquire_many_owned(extra)).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(_)) | Err(_) => return Err(OverBudget { needed: bytes }),
                }
            }
        };
        self.permit.merge(more);
        Ok(())
    }

    /// Hold at least `bytes` (capped at the whole budget) only if the extra
    /// is available right now.
    pub fn try_grow_to(&mut self, bytes: usize) -> Result<(), OverBudget> {
        let want = self.budget.units_for(bytes);
        let held = u32::try_from(self.permit.num_permits()).unwrap_or(u32::MAX);
        if want <= held {
            return Ok(());
        }
        let more = Arc::clone(&self.budget.sem)
            .try_acquire_many_owned(want - held)
            .map_err(|_no_room| OverBudget { needed: bytes })?;
        self.permit.merge(more);
        Ok(())
    }

    /// Hold exactly what `bytes` needs (capped at the whole budget),
    /// returning any excess to the budget. Never grows.
    pub fn shrink_to(&mut self, bytes: usize) {
        let want = self.budget.units_for(bytes) as usize;
        let held = self.permit.num_permits();
        if want < held {
            drop(self.permit.split(held - want));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn reservations_are_returned_on_drop() {
        rt().block_on(async {
            let b = Arc::new(Budget::new(10 * UNIT));
            let (r, waited) = b.reserve(4 * UNIT).await.expect("room");
            assert!(!waited);
            assert_eq!(r.bytes(), 4 * UNIT);
            assert_eq!(b.available_bytes(), 6 * UNIT);
            drop(r);
            assert_eq!(b.available_bytes(), 10 * UNIT);
        });
    }

    #[test]
    fn oversized_request_takes_the_whole_budget() {
        rt().block_on(async {
            let b = Arc::new(Budget::new(10 * UNIT));
            let (r, _) = b.reserve(1 << 30).await.expect("room");
            assert_eq!(r.bytes(), 10 * UNIT);
            assert_eq!(b.available_bytes(), 0);
        });
    }

    #[test]
    fn growth_times_out_instead_of_deadlocking() {
        rt().block_on(async {
            let b = Arc::new(Budget::new(10 * UNIT));
            let (mut a, _) = b.reserve(5 * UNIT).await.expect("room");
            let (mut c, _) = b.reserve(5 * UNIT).await.expect("room");
            let deadline = Instant::now() + Duration::from_millis(20);
            let err = a.grow_to(6 * UNIT, deadline).await.expect_err("no room");
            assert_eq!(err.needed, 6 * UNIT);
            // Partial holdings are kept; nothing leaked.
            assert_eq!(a.bytes() + c.bytes(), 10 * UNIT);
            c.shrink_to(2 * UNIT);
            assert_eq!(b.available_bytes(), 3 * UNIT);
            a.grow_to(8 * UNIT, deadline).await.expect("room now");
            assert_eq!(a.bytes(), 8 * UNIT);
            // Growth without waiting fails at once when there is no room.
            assert_eq!(a.try_grow_to(9 * UNIT).expect_err("full").needed, 9 * UNIT);
            c.shrink_to(UNIT);
            a.try_grow_to(9 * UNIT).expect("room now");
            assert_eq!(b.available_bytes(), 0);
        });
    }

    #[test]
    fn reserve_waits_until_room_is_released() {
        rt().block_on(async {
            let b = Arc::new(Budget::new(4 * UNIT));
            let (held, _) = b.reserve(4 * UNIT).await.expect("room");
            let b2 = Arc::clone(&b);
            let waiter = tokio::spawn(async move { b2.reserve(2 * UNIT).await });
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert!(!waiter.is_finished());
            drop(held);
            let (r, waited) = waiter.await.expect("join").expect("room");
            assert!(waited);
            assert_eq!(r.bytes(), 2 * UNIT);
        });
    }
}
