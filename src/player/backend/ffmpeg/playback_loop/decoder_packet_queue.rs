use super::*;

pub(super) struct DecoderPacketQueues<P, const PENDING_CAPACITY: usize> {
    pending_input: VecDeque<P>,
    in_flight: VecDeque<P>,
}

impl<P, const PENDING_CAPACITY: usize> Default for DecoderPacketQueues<P, PENDING_CAPACITY> {
    fn default() -> Self {
        Self {
            pending_input: VecDeque::new(),
            in_flight: VecDeque::new(),
        }
    }
}

impl<P, const PENDING_CAPACITY: usize> DecoderPacketQueues<P, PENDING_CAPACITY> {
    pub(super) fn clear(&mut self) {
        self.pending_input.clear();
        self.in_flight.clear();
    }

    pub(super) fn has_pending_or_in_flight(&self) -> bool {
        !self.pending_input.is_empty() || !self.in_flight.is_empty()
    }

    pub(super) fn has_pending_input(&self) -> bool {
        !self.pending_input.is_empty()
    }

    pub(super) fn pending_input_count(&self) -> usize {
        self.pending_input.len()
    }

    pub(super) fn pending_input_capacity(&self) -> usize {
        PENDING_CAPACITY
    }

    pub(super) fn pending_input_full(&self) -> bool {
        self.pending_input.len() >= PENDING_CAPACITY
    }

    pub(super) fn push_pending_input(&mut self, packet: P) -> std::result::Result<(), P> {
        if self.pending_input_full() {
            return Err(packet);
        }
        self.pending_input.push_back(packet);
        Ok(())
    }

    pub(super) fn push_pending_input_front(&mut self, packet: P) {
        self.pending_input.push_front(packet);
    }

    pub(super) fn take_pending_input(&mut self) -> Option<P> {
        self.pending_input.pop_front()
    }

    pub(super) fn push_in_flight(&mut self, packet: P) {
        self.in_flight.push_back(packet);
    }

    pub(super) fn front_in_flight(&self) -> Option<&P> {
        self.in_flight.front()
    }

    pub(super) fn pop_completed_packet(&mut self) -> Option<P> {
        self.in_flight.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_packet_queues_preserve_pending_fifo_order() {
        let mut queue = DecoderPacketQueues::<u32, 3>::default();

        queue.push_pending_input(1).unwrap();
        queue.push_pending_input(2).unwrap();
        queue.push_pending_input(3).unwrap();

        assert!(queue.pending_input_full());
        assert!(queue.push_pending_input(4).is_err());
        assert_eq!(queue.take_pending_input(), Some(1));
        queue.push_pending_input(4).unwrap();
        assert_eq!(queue.take_pending_input(), Some(2));
        assert_eq!(queue.take_pending_input(), Some(3));
        assert_eq!(queue.take_pending_input(), Some(4));
        assert_eq!(queue.take_pending_input(), None);
    }

    #[test]
    fn decoder_packet_queues_requeue_front_preserves_oldest_packet() {
        let mut queue = DecoderPacketQueues::<u32, 3>::default();

        queue.push_pending_input(2).unwrap();
        queue.push_pending_input(3).unwrap();
        queue.push_pending_input_front(1);

        assert_eq!(queue.take_pending_input(), Some(1));
        assert_eq!(queue.take_pending_input(), Some(2));
        assert_eq!(queue.take_pending_input(), Some(3));
    }
}
