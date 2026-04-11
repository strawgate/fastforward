//! Worker delivery outcome -> checkpoint policy mapping.
//!
//! This module keeps checkpoint advancement policy explicit at the
//! worker-to-pipeline seam.

use crate::worker_pool::DeliveryOutcome;

/// How pipeline tickets should be finalized for a worker outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TicketDisposition {
    Ack,
    Reject,
    Hold,
}

impl TicketDisposition {
    #[cfg(any(test, kani))]
    const fn from_repr(value: u8) -> Self {
        match value {
            0 => Self::Ack,
            1 => Self::Reject,
            _ => Self::Hold,
        }
    }

    #[cfg(any(test, kani))]
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Ack | Self::Reject)
    }
}

/// Delivery outcome -> checkpoint policy for `#1520`.
///
/// Policy:
/// - successful delivery ACKs tickets
/// - explicit permanent rejection rejects tickets and advances the checkpoint
/// - all other failures hold tickets unresolved so the checkpoint does not
///   advance past undelivered data
///
/// Hold is intentionally conservative. The current runtime does not yet retain
/// enough batch payload state to requeue these batches in-process, so the
/// immediate effect is "do not advance; replay on restart if shutdown forces a
/// stop while these tickets remain unresolved."
#[must_use]
pub(super) const fn default_ticket_disposition(outcome: &DeliveryOutcome) -> TicketDisposition {
    match outcome {
        DeliveryOutcome::Delivered => TicketDisposition::Ack,
        DeliveryOutcome::Rejected { .. } => TicketDisposition::Reject,
        DeliveryOutcome::RetryExhausted
        | DeliveryOutcome::TimedOut
        | DeliveryOutcome::PoolClosed
        | DeliveryOutcome::WorkerChannelClosed
        | DeliveryOutcome::NoWorkersAvailable
        | DeliveryOutcome::InternalFailure => TicketDisposition::Hold,
    }
}

#[cfg(any(test, kani))]
const ORDERED_TICKET_WINDOW: u8 = 8;

/// Bounded pure model of ordered checkpoint advancement for one source.
///
/// The runtime's real checkpoint state lives in `PipelineMachine`; this local
/// seam model keeps the worker-outcome policy proof small and explicit.
#[cfg(any(test, kani))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OrderedCheckpointState {
    ticket_count: u8,
    committed_count: u8,
    terminal_mask: u16,
}

#[cfg(any(test, kani))]
impl OrderedCheckpointState {
    pub(super) const fn new(ticket_count: u8) -> Self {
        Self::from_terminal_mask(ticket_count, 0)
    }

    pub(super) const fn from_terminal_mask(ticket_count: u8, terminal_mask: u16) -> Self {
        let ticket_count = if ticket_count > ORDERED_TICKET_WINDOW {
            ORDERED_TICKET_WINDOW
        } else {
            ticket_count
        };
        let terminal_mask = terminal_mask & Self::valid_mask(ticket_count);
        let committed_count = Self::contiguous_prefix(ticket_count, terminal_mask);

        Self {
            ticket_count,
            committed_count,
            terminal_mask,
        }
    }

    pub(super) const fn apply(self, ticket_index: u8, disposition: TicketDisposition) -> Self {
        if ticket_index >= self.ticket_count || !disposition.is_terminal() {
            return self;
        }

        let terminal_mask = self.terminal_mask | (1u16 << ticket_index);
        Self::from_terminal_mask(self.ticket_count, terminal_mask)
    }

    pub(super) const fn ticket_count(self) -> u8 {
        self.ticket_count
    }

    pub(super) const fn committed_count(self) -> u8 {
        self.committed_count
    }

    #[cfg(kani)]
    pub(super) const fn terminal_mask(self) -> u16 {
        self.terminal_mask
    }

    pub(super) const fn has_unresolved_gap_before_commit(self) -> bool {
        let mut index = 0;
        while index < self.committed_count {
            if !Self::is_terminal_at(self.terminal_mask, index) {
                return true;
            }
            index += 1;
        }
        false
    }

    pub(super) const fn has_committed_past_unresolved_gap(self) -> bool {
        if self.committed_count >= self.ticket_count {
            return false;
        }
        Self::is_terminal_at(self.terminal_mask, self.committed_count)
    }

    const fn valid_mask(ticket_count: u8) -> u16 {
        if ticket_count == 0 {
            0
        } else {
            (1u16 << ticket_count) - 1
        }
    }

    const fn contiguous_prefix(ticket_count: u8, terminal_mask: u16) -> u8 {
        let mut committed = 0;
        while committed < ticket_count {
            if !Self::is_terminal_at(terminal_mask, committed) {
                break;
            }
            committed += 1;
        }
        committed
    }

    const fn is_terminal_at(terminal_mask: u16, index: u8) -> bool {
        (terminal_mask & (1u16 << index)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{OrderedCheckpointState, TicketDisposition, default_ticket_disposition};
    use crate::worker_pool::DeliveryOutcome;
    use proptest::prelude::*;

    fn expected_committed_prefix(ticket_count: u8, terminal: &[bool]) -> u8 {
        let mut committed = 0;
        while committed < ticket_count {
            if !terminal[usize::from(committed)] {
                break;
            }
            committed += 1;
        }
        committed
    }

    #[test]
    fn delivered_acks_tickets() {
        assert_eq!(
            default_ticket_disposition(&DeliveryOutcome::Delivered),
            TicketDisposition::Ack
        );
    }

    #[test]
    fn explicit_rejects_advance_checkpoints() {
        let rejected = DeliveryOutcome::Rejected {
            reason: "bad request".to_string(),
        };
        assert_eq!(
            default_ticket_disposition(&rejected),
            TicketDisposition::Reject
        );
    }

    #[test]
    fn control_plane_and_retry_failures_hold_tickets() {
        for outcome in [
            DeliveryOutcome::RetryExhausted,
            DeliveryOutcome::TimedOut,
            DeliveryOutcome::PoolClosed,
            DeliveryOutcome::WorkerChannelClosed,
            DeliveryOutcome::NoWorkersAvailable,
            DeliveryOutcome::InternalFailure,
        ] {
            assert_eq!(
                default_ticket_disposition(&outcome),
                TicketDisposition::Hold
            );
        }
    }

    #[test]
    fn hold_leaves_gap_and_blocks_later_terminal_tickets() {
        let mut state = OrderedCheckpointState::new(3);

        state = state.apply(0, TicketDisposition::Hold);
        assert_eq!(state.committed_count(), 0);

        state = state.apply(1, TicketDisposition::Ack);
        assert_eq!(state.committed_count(), 0);

        state = state.apply(2, TicketDisposition::Reject);
        assert_eq!(state.committed_count(), 0);

        state = state.apply(0, TicketDisposition::Ack);
        assert_eq!(state.committed_count(), 3);
    }

    #[test]
    fn ack_and_reject_have_identical_ordered_commit_semantics() {
        let state = OrderedCheckpointState::from_terminal_mask(4, 0b0101);

        assert_eq!(
            state.apply(1, TicketDisposition::Ack),
            state.apply(1, TicketDisposition::Reject)
        );
    }

    proptest! {
        #[test]
        fn checkpoint_policy_preserves_ordered_commit_invariants(
            ticket_count in 0u8..=8,
            events in proptest::collection::vec((0u8..=10, 0u8..=2), 0..256),
        ) {
            let mut state = OrderedCheckpointState::new(ticket_count);
            let mut terminal = vec![false; usize::from(state.ticket_count())];

            for (ticket_index, raw_disposition) in events {
                let disposition = TicketDisposition::from_repr(raw_disposition);
                let previous = state;
                state = state.apply(ticket_index, disposition);

                if disposition.is_terminal() && ticket_index < state.ticket_count() {
                    terminal[usize::from(ticket_index)] = true;
                }

                prop_assert!(
                    state.committed_count() >= previous.committed_count(),
                    "checkpoint commit must be monotonic"
                );
                prop_assert!(
                    state.committed_count() <= state.ticket_count(),
                    "checkpoint commit cannot move beyond known tickets"
                );
                prop_assert!(
                    !state.has_unresolved_gap_before_commit(),
                    "checkpoint commit cannot cross an unresolved earlier ticket"
                );
                prop_assert!(
                    !state.has_committed_past_unresolved_gap(),
                    "checkpoint commit must stop at the first unresolved ticket"
                );
                prop_assert_eq!(
                    state.committed_count(),
                    expected_committed_prefix(state.ticket_count(), &terminal),
                    "checkpoint commit must equal the independent contiguous-prefix oracle"
                );

                if matches!(disposition, TicketDisposition::Hold) {
                    prop_assert_eq!(
                        state,
                        previous,
                        "hold must not resolve a ticket or advance the checkpoint"
                    );
                }
            }
        }

        #[test]
        fn ack_and_reject_are_exact_terminal_equivalents(
            ticket_count in 0u8..=8,
            terminal_mask in any::<u16>(),
            ticket_index in 0u8..=10,
        ) {
            let state = OrderedCheckpointState::from_terminal_mask(ticket_count, terminal_mask);

            prop_assert_eq!(
                state.apply(ticket_index, TicketDisposition::Ack),
                state.apply(ticket_index, TicketDisposition::Reject),
                "ack and reject must have identical checkpoint-advance semantics"
            );
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::default_ticket_disposition;
    use super::{ORDERED_TICKET_WINDOW, OrderedCheckpointState, TicketDisposition};
    use crate::worker_pool::DeliveryOutcome;

    #[kani::proof]
    fn verify_default_ticket_disposition_delivered_acks() {
        assert_eq!(
            default_ticket_disposition(&DeliveryOutcome::Delivered),
            TicketDisposition::Ack
        );
        kani::cover!(true, "delivered ack path exercised");
    }

    #[kani::proof]
    fn verify_default_ticket_disposition_rejected_advances() {
        let rejected = DeliveryOutcome::Rejected {
            reason: "bad request".to_owned(),
        };
        assert_eq!(
            default_ticket_disposition(&rejected),
            TicketDisposition::Reject
        );
        kani::cover!(true, "explicit reject path exercised");
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn verify_default_ticket_disposition_non_terminal_failures_hold() {
        for outcome in [
            DeliveryOutcome::RetryExhausted,
            DeliveryOutcome::TimedOut,
            DeliveryOutcome::PoolClosed,
            DeliveryOutcome::WorkerChannelClosed,
            DeliveryOutcome::NoWorkersAvailable,
            DeliveryOutcome::InternalFailure,
        ] {
            assert_eq!(
                default_ticket_disposition(&outcome),
                TicketDisposition::Hold
            );
        }
        kani::cover!(true, "non-terminal failure hold paths exercised");
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn verify_ordered_checkpoint_state_hold_never_advances() {
        let ticket_count: u8 = kani::any_where(|count| *count <= ORDERED_TICKET_WINDOW);
        let terminal_mask: u16 = kani::any();
        let ticket_index: u8 = kani::any_where(|index| *index <= ORDERED_TICKET_WINDOW + 1);

        let before = OrderedCheckpointState::from_terminal_mask(ticket_count, terminal_mask);
        let after = before.apply(ticket_index, TicketDisposition::Hold);

        assert_eq!(after, before);
        kani::cover!(
            ticket_index < before.ticket_count(),
            "hold of tracked ticket is exercised"
        );
        kani::cover!(
            ticket_index >= before.ticket_count(),
            "hold of out-of-window ticket is exercised"
        );
        kani::cover!(
            before.committed_count() < before.ticket_count(),
            "hold with unresolved checkpoint gap is exercised"
        );
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn verify_ordered_checkpoint_state_ack_reject_exact_terminal_semantics() {
        let ticket_count: u8 = kani::any_where(|count| *count <= ORDERED_TICKET_WINDOW);
        let terminal_mask: u16 = kani::any();
        let ticket_index: u8 = kani::any_where(|index| *index <= ORDERED_TICKET_WINDOW + 1);

        let before = OrderedCheckpointState::from_terminal_mask(ticket_count, terminal_mask);
        let ack = before.apply(ticket_index, TicketDisposition::Ack);
        let reject = before.apply(ticket_index, TicketDisposition::Reject);

        assert_eq!(ack, reject);
        assert!(ack.committed_count() >= before.committed_count());
        assert!(ack.committed_count() <= ack.ticket_count());
        assert!(!ack.has_unresolved_gap_before_commit());
        assert!(!ack.has_committed_past_unresolved_gap());

        if ticket_index < before.ticket_count() {
            assert_eq!(
                ack.terminal_mask(),
                before.terminal_mask() | (1u16 << ticket_index)
            );
        } else {
            assert_eq!(ack, before);
        }

        kani::cover!(
            ticket_index < before.ticket_count()
                && ack.committed_count() > before.committed_count(),
            "terminal receipt advances a contiguous checkpoint"
        );
        kani::cover!(
            ticket_index < before.ticket_count()
                && ack.committed_count() == before.committed_count(),
            "terminal receipt is buffered behind an unresolved gap"
        );
        kani::cover!(
            ticket_index >= before.ticket_count(),
            "out-of-window terminal receipt is ignored"
        );
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn verify_ordered_checkpoint_state_mixed_sequence_monotonic_without_illegal_jumps() {
        const SEQUENCE_LEN: usize = 5;

        let ticket_count: u8 = kani::any_where(|count| *count <= ORDERED_TICKET_WINDOW);
        let ticket_indexes: [u8; SEQUENCE_LEN] = kani::any();
        let raw_dispositions: [u8; SEQUENCE_LEN] = kani::any();
        let mut state = OrderedCheckpointState::new(ticket_count);
        let mut step = 0;

        while step < SEQUENCE_LEN {
            let previous = state;
            let ticket_index = ticket_indexes[step] % (ORDERED_TICKET_WINDOW + 2);
            let disposition = TicketDisposition::from_repr(raw_dispositions[step]);

            state = state.apply(ticket_index, disposition);

            assert!(state.committed_count() >= previous.committed_count());
            assert!(state.committed_count() <= state.ticket_count());
            assert!(!state.has_unresolved_gap_before_commit());
            assert!(!state.has_committed_past_unresolved_gap());

            if matches!(disposition, TicketDisposition::Hold) {
                assert_eq!(state, previous);
            }

            kani::cover!(
                matches!(disposition, TicketDisposition::Ack),
                "mixed sequence includes ack"
            );
            kani::cover!(
                matches!(disposition, TicketDisposition::Reject),
                "mixed sequence includes reject"
            );
            kani::cover!(
                matches!(disposition, TicketDisposition::Hold),
                "mixed sequence includes hold"
            );
            kani::cover!(
                state.committed_count() > previous.committed_count(),
                "mixed sequence advances checkpoint"
            );
            kani::cover!(
                disposition.is_terminal()
                    && ticket_index < state.ticket_count()
                    && state.committed_count() == previous.committed_count(),
                "mixed sequence buffers terminal receipt behind gap"
            );

            step += 1;
        }
    }
}
