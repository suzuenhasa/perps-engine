//! The single-producer, single-consumer ring that connects every pair of pipeline threads
//! (`docs/PIPELINE.md` 3.1 and 3.2; `docs/DECISIONS.md` D-024). Our own, in safe Rust.
//!
//! **Contract.** [`channel`] returns the two ends of a bounded FIFO of fixed-size records.
//! A record is up to `8 * LINES` words (`u64`). The [`Producer`] writes records and then
//! publishes them; the [`Consumer`] sees published records in the order they were written,
//! each exactly once, and frees their slots when it releases them. Nothing is ever dropped
//! or overwritten: the producer can only write into a slot the consumer has released. Each
//! end belongs to one thread at a time (both are `Send`, neither is `Clone`), and neither
//! side ever blocks: `free` and `available` just report, and the caller decides whether to
//! wait, drop or reject (PIPELINE.md 2.4).
//!
//! **Layout.** Every slot is `LINES` cache lines ([`Line`]: eight `AtomicU64`, 64-byte
//! aligned), so two slots never share a line. Two counters say where the ring is:
//! - `tail`: records the producer has published since the ring was created;
//! - `head`: records the consumer has released since the ring was created.
//!
//! They never wrap (2^64 records at a billion a second would take 584 years), so the ring
//! holds `tail - head` records, and record `i` lives in slot `i & (capacity - 1)`. Each
//! counter is alone on its own 128 bytes ([`CachePadded`]), so the producer's and the
//! consumer's writes never touch the same line.
//!
//! **Memory ordering** (PIPELINE.md 3.2, the whole argument):
//! - The producer writes a record's words (Relaxed), then stores the new `tail` (Release).
//!   The consumer loads `tail` (Acquire), then reads the words (Relaxed). Release/Acquire
//!   means: once the consumer sees the new `tail`, it sees every word written before it.
//! - The consumer stores the new `head` (Release) after copying the words out. The
//!   producer loads `head` (Acquire) before it reuses a slot. So the producer never
//!   overwrites a slot that the consumer is still reading.
//! - Closing: dropping the producer stores `tail` (Release), then `closed = true`
//!   (Release). The consumer loads `closed` (Acquire) first, then `tail` (Acquire). If it
//!   sees `closed`, it also sees the final `tail`, so "closed and empty" really means that
//!   nothing more will come. Loading them in the other order could miss a last record
//!   published between the two loads.
//!
//! No locks and no compare-and-swap: each counter has exactly one writer.
//!
//! **Why atomic words and not a ring of `T`.** A slot of `AtomicU64` can be written by one
//! thread while another reads it without `unsafe`, and a reader can never see half of a
//! word. On x86 a Relaxed load or store is a plain `mov`, so this costs nothing over plain
//! memory. The price is that records are encoded into words field by field, which the
//! journal needs anyway (D-005, `records.rs`).
//!
//! **Cached counters.** Each side keeps a private copy of the other side's counter and
//! reloads it only when its copy can't satisfy what the caller asked for (the `wanted`
//! argument). So in the common case neither side reads the other's cache line, and a stale
//! copy never makes a batch smaller than the caller wanted.
//!
//! **Pre-touched.** [`channel`] writes every word of every slot once before returning, so
//! every page of the ring is mapped before a run starts, and the first lap round the ring
//! takes no page faults inside a measured window (PIPELINE.md 3.1, 15.4).
//!
//! **Complexity.** Every call is O(1), apart from `write` and `read`, which are O(words in
//! the record). `free`, `available` and `is_finished` touch the other side's counter only
//! when the cached copy is not enough; `publish` and `release` are one store each.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Words per [`Line`]: 8 × 8 bytes = one 64-byte cache line.
pub const WORDS_PER_LINE: usize = 8;

/// One cache line of eight atomic words. The alignment makes every slot start on a cache
/// line (D-005's promise), so two slots never share a line.
#[repr(align(64))]
#[derive(Debug)]
pub struct Line(pub [AtomicU64; WORDS_PER_LINE]);

impl Line {
    fn zeroed() -> Line {
        Line(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

/// A value alone on its cache lines. 128 bytes, not 64, because Intel's adjacent-line
/// prefetcher fetches lines in pairs, so two counters 64 bytes apart would still interfere.
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct CachePadded<T>(pub T);

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        CachePadded(value)
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// What the two ends share. See the module docs for who writes what.
struct Shared<const LINES: usize> {
    /// `capacity` slots, allocated and pre-touched by [`channel`].
    slots: Box<[[Line; LINES]]>,
    /// Records the consumer has released. Written only by the consumer.
    head: CachePadded<AtomicU64>,
    /// Records the producer has published. Written only by the producer.
    tail: CachePadded<AtomicU64>,
    /// Set once, by the producer, when it is dropped (after its last publish).
    closed: CachePadded<AtomicBool>,
}

impl<const LINES: usize> Shared<LINES> {
    /// Word `w` of the slot that holds record number `record`.
    fn word(&self, record: u64, w: usize) -> &AtomicU64 {
        // The slot index is `record mod capacity`; capacity is a power of two.
        let slot = &self.slots[(record & (self.slots.len() as u64 - 1)) as usize];
        &slot[w / WORDS_PER_LINE].0[w % WORDS_PER_LINE]
    }
}

/// Creates a ring of `capacity` slots of `LINES` cache lines each, and returns its two ends.
///
/// Panics unless `capacity` is a power of two (the slot index is `i & (capacity - 1)`), or
/// if `LINES` is 0.
pub fn channel<const LINES: usize>(capacity: usize) -> (Producer<LINES>, Consumer<LINES>) {
    assert!(capacity.is_power_of_two(), "ring capacity must be a power of two, got {capacity}");
    assert!(LINES > 0, "a ring slot needs at least one line");
    let slots: Box<[[Line; LINES]]> =
        (0..capacity).map(|_| std::array::from_fn(|_| Line::zeroed())).collect();
    pre_touch(&slots);
    let shared = Arc::new(Shared {
        slots,
        head: CachePadded::new(AtomicU64::new(0)),
        tail: CachePadded::new(AtomicU64::new(0)),
        closed: CachePadded::new(AtomicBool::new(false)),
    });
    let producer = Producer { shared: Arc::clone(&shared), written: 0, published: 0, cached_head: 0 };
    let consumer = Consumer { shared, read: 0, released: 0, cached_tail: 0 };
    (producer, consumer)
}

/// Writes every word once, a nonzero value and then zero, so that every page of the ring
/// is mapped now rather than on the first lap of a run. A fresh zeroed allocation can come
/// straight from the OS as untouched pages; storing only zeros could, in principle, be
/// optimised away as storing what is already there, hence the nonzero value first.
fn pre_touch<const LINES: usize>(slots: &[[Line; LINES]]) {
    for word in slots.iter().flatten().flat_map(|line| &line.0) {
        word.store(u64::MAX, Ordering::Relaxed);
        word.store(0, Ordering::Relaxed);
    }
}

/// The writing end of a ring. See the module docs.
pub struct Producer<const LINES: usize> {
    shared: Arc<Shared<LINES>>,
    /// Records written so far (the next record's number). Published up to `published`.
    written: u64,
    /// The last value stored into `tail`.
    published: u64,
    /// The last value loaded from `head`: never ahead of the real one.
    cached_head: u64,
}

impl<const LINES: usize> Producer<LINES> {
    /// Slots in the ring.
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Free slots. Reloads `head` (Acquire) only if the cached view shows fewer than
    /// `wanted` free, so the answer is at least `wanted` whenever that many are really
    /// free, and may be less than the true count otherwise.
    pub fn free(&mut self, wanted: usize) -> usize {
        let mut free = self.cached_free();
        if free < wanted {
            self.cached_head = self.shared.head.load(Ordering::Acquire);
            free = self.cached_free();
        }
        free
    }

    fn cached_free(&self) -> usize {
        self.capacity() - (self.written - self.cached_head) as usize
    }

    /// Writes one record into the next free slot, not yet visible to the consumer (see
    /// [`Producer::publish`]). Words of the slot beyond `words.len()` keep whatever an
    /// earlier record left there, so the consumer must read only what was written.
    ///
    /// Panics if there is no free slot (callers check [`Producer::free`] first) or if
    /// `words.len() > 8 * LINES`.
    pub fn write(&mut self, words: &[u64]) {
        assert!(
            words.len() <= WORDS_PER_LINE * LINES,
            "a record of {} words doesn't fit a {LINES}-line slot",
            words.len()
        );
        assert!(self.free(1) > 0, "write into a full ring: check free() first");
        for (w, &word) in words.iter().enumerate() {
            self.shared.word(self.written, w).store(word, Ordering::Relaxed);
        }
        self.written += 1;
    }

    /// Makes every record written so far visible to the consumer: one Release store of
    /// `tail`. Does nothing if nothing was written since the last publish, so the
    /// consumer's copy of the line holding `tail` isn't invalidated for nothing.
    pub fn publish(&mut self) {
        if self.written != self.published {
            self.shared.tail.store(self.written, Ordering::Release);
            self.published = self.written;
        }
    }
}

/// Dropping the producer publishes anything written, then closes the ring (Release). This
/// is how the pipeline stops, in data-flow order (PIPELINE.md 2.8).
impl<const LINES: usize> Drop for Producer<LINES> {
    fn drop(&mut self) {
        self.publish();
        self.shared.closed.store(true, Ordering::Release);
    }
}

impl<const LINES: usize> fmt::Debug for Producer<LINES> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Producer")
            .field("lines", &LINES)
            .field("capacity", &self.capacity())
            .field("written", &self.written)
            .field("published", &self.published)
            .field("cached_head", &self.cached_head)
            .finish()
    }
}

/// The reading end of a ring. See the module docs.
pub struct Consumer<const LINES: usize> {
    shared: Arc<Shared<LINES>>,
    /// Records read so far (the next record's number). Released up to `released`.
    read: u64,
    /// The last value stored into `head`.
    released: u64,
    /// The last value loaded from `tail`: never ahead of the real one.
    cached_tail: u64,
}

impl<const LINES: usize> Consumer<LINES> {
    /// Slots in the ring.
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Published records not yet read. Reloads `tail` (Acquire) only if the cached view
    /// shows fewer than `wanted`, so the answer is at least `wanted` whenever that many are
    /// really published, and may be less than the true count otherwise.
    pub fn available(&mut self, wanted: usize) -> usize {
        let mut available = self.cached_available();
        if available < wanted {
            self.cached_tail = self.shared.tail.load(Ordering::Acquire);
            available = self.cached_available();
        }
        available
    }

    fn cached_available(&self) -> usize {
        (self.cached_tail - self.read) as usize
    }

    /// Word `w` of the next unread record, without moving on.
    ///
    /// Panics unless [`Consumer::available`] has already reported that record, or if `w` is
    /// outside the slot.
    pub fn peek(&self, w: usize) -> u64 {
        assert!(self.cached_available() > 0, "peek with no record available: call available() first");
        assert!(w < WORDS_PER_LINE * LINES, "word {w} is outside a {LINES}-line slot");
        self.shared.word(self.read, w).load(Ordering::Relaxed)
    }

    /// Word `w` of the unread record `k` places after the next one (`peek_at(0, w)` is
    /// `peek(w)`), without moving on. The gate uses it to look at the last record published
    /// so far (12.1).
    ///
    /// Panics unless [`Consumer::available`] has already reported at least `k + 1`
    /// records, or if `w` is outside the slot.
    pub fn peek_at(&self, k: usize, w: usize) -> u64 {
        let available = self.cached_available();
        assert!(k < available, "peek at record {k} with {available} available: call available() first");
        assert!(w < WORDS_PER_LINE * LINES, "word {w} is outside a {LINES}-line slot");
        self.shared.word(self.read + k as u64, w).load(Ordering::Relaxed)
    }

    /// True once the producer has closed the ring: nothing will be published after what
    /// [`Consumer::available`] reports from now on. Loads `closed` (Acquire); a caller that
    /// then calls `available` sees the final `tail`, for the reason the module docs give.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Copies the first `out.len()` words of the next unread record into `out`, and moves
    /// the read cursor on. The slot stays reserved until [`Consumer::release`].
    ///
    /// Panics if no record is published, or if `out.len() > 8 * LINES`.
    pub fn read(&mut self, out: &mut [u64]) {
        assert!(
            out.len() <= WORDS_PER_LINE * LINES,
            "can't read {} words from a {LINES}-line slot",
            out.len()
        );
        assert!(self.available(1) > 0, "read from an empty ring: check available() first");
        for (w, word) in out.iter_mut().enumerate() {
            *word = self.shared.word(self.read, w).load(Ordering::Relaxed);
        }
        self.read += 1;
    }

    /// Frees every slot read so far, for the producer to reuse: one Release store of
    /// `head`. Does nothing if nothing was read since the last release.
    pub fn release(&mut self) {
        if self.read != self.released {
            self.shared.head.store(self.read, Ordering::Release);
            self.released = self.read;
        }
    }

    /// True once the producer has closed the ring and every record has been read. Loads
    /// `closed` (Acquire) first, then reloads `tail` (Acquire): see the module docs for why
    /// the order matters.
    pub fn is_finished(&mut self) -> bool {
        if !self.shared.closed.load(Ordering::Acquire) {
            return false;
        }
        self.cached_tail = self.shared.tail.load(Ordering::Acquire);
        self.read == self.cached_tail
    }
}

impl<const LINES: usize> fmt::Debug for Consumer<LINES> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Consumer")
            .field("lines", &LINES)
            .field("capacity", &self.capacity())
            .field("read", &self.read)
            .field("released", &self.released)
            .field("cached_tail", &self.cached_tail)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record whose every word depends on its number, so a stale or torn record shows.
    fn record(i: u64, words: usize) -> Vec<u64> {
        (0..words as u64).map(|w| (i << 8) | w).collect()
    }

    fn read_one<const LINES: usize>(consumer: &mut Consumer<LINES>, words: usize) -> Vec<u64> {
        let mut out = vec![0; words];
        consumer.read(&mut out);
        out
    }

    #[test]
    fn line_and_padding_have_the_promised_sizes() {
        assert_eq!(size_of::<Line>(), 64);
        assert_eq!(align_of::<Line>(), 64);
        assert_eq!(size_of::<[Line; 3]>(), 192);
        assert_eq!(size_of::<CachePadded<AtomicU64>>(), 128);
        assert_eq!(align_of::<CachePadded<AtomicBool>>(), 128);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn a_capacity_that_is_not_a_power_of_two_panics() {
        let _ = channel::<1>(3);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn a_capacity_of_zero_panics() {
        let _ = channel::<1>(0);
    }

    #[test]
    fn a_capacity_one_ring_goes_through_full_empty_and_wrap_around() {
        let (mut producer, mut consumer) = channel::<1>(1);
        for i in 0..5 {
            assert_eq!(producer.free(1), 1, "lap {i}");
            assert_eq!(consumer.available(1), 0, "lap {i}");
            producer.write(&record(i, 8));
            assert_eq!(producer.free(1), 0, "lap {i}: full once written");
            assert_eq!(consumer.available(1), 0, "lap {i}: invisible until published");
            producer.publish();
            assert_eq!(consumer.available(1), 1, "lap {i}");
            assert_eq!(read_one(&mut consumer, 8), record(i, 8), "lap {i}");
            assert_eq!(producer.free(1), 0, "lap {i}: still full until released");
            consumer.release();
            assert_eq!(consumer.available(1), 0, "lap {i}");
        }
    }

    #[test]
    fn a_capacity_two_ring_goes_through_full_empty_and_wrap_around() {
        let (mut producer, mut consumer) = channel::<2>(2);
        let mut next_write = 0;
        let mut next_read = 0;
        for lap in 0..4 {
            // Fill it.
            while producer.free(1) > 0 {
                producer.write(&record(next_write, 16));
                next_write += 1;
            }
            producer.publish();
            assert_eq!(next_write - next_read, 2, "lap {lap}: full at two records");
            assert_eq!(consumer.available(2), 2, "lap {lap}");
            // Read one, release it, and write one more into its slot: the ring wraps.
            assert_eq!(read_one(&mut consumer, 16), record(next_read, 16));
            next_read += 1;
            consumer.release();
            assert_eq!(producer.free(1), 1, "lap {lap}");
            producer.write(&record(next_write, 16));
            next_write += 1;
            producer.publish();
            // Empty it.
            while consumer.available(1) > 0 {
                assert_eq!(read_one(&mut consumer, 16), record(next_read, 16), "lap {lap}");
                next_read += 1;
            }
            consumer.release();
            assert_eq!(producer.free(2), 2, "lap {lap}: empty again");
        }
    }

    #[test]
    fn free_and_available_are_right_at_every_fill_level() {
        const CAPACITY: usize = 8;
        let (mut producer, mut consumer) = channel::<1>(CAPACITY);
        // Walk the fill level up to full and back down, several times, so the counters
        // cross the slot index's wrap-around.
        let mut written = 0;
        for _ in 0..3 {
            for fill in 0..=CAPACITY {
                assert_eq!(producer.free(CAPACITY), CAPACITY - fill, "free at fill {fill}");
                assert_eq!(consumer.available(CAPACITY), fill, "available at fill {fill}");
                if fill < CAPACITY {
                    producer.write(&record(written, 1));
                    written += 1;
                    producer.publish();
                }
            }
            for fill in (0..CAPACITY).rev() {
                read_one(&mut consumer, 1);
                consumer.release();
                assert_eq!(producer.free(CAPACITY), CAPACITY - fill, "free at fill {fill} going down");
                assert_eq!(consumer.available(CAPACITY), fill, "available at fill {fill} going down");
            }
        }
    }

    #[test]
    fn a_stale_cached_count_is_reloaded_only_when_below_wanted() {
        let (mut producer, mut consumer) = channel::<1>(4);
        producer.write(&[1]);
        producer.publish();
        assert_eq!(consumer.available(1), 1); // loads tail: 1
        producer.write(&[2]);
        producer.write(&[3]);
        producer.publish();
        // The cached view (1) satisfies wanted = 1, so the answer stays 1: no reload.
        assert_eq!(consumer.available(1), 1);
        // Asking for more than the cached view reloads and sees all three.
        assert_eq!(consumer.available(2), 3);

        // The same on the producer side: 4 free, then 1 after three more writes.
        let mut out = [0];
        consumer.read(&mut out);
        consumer.release();
        assert_eq!(producer.free(1), 1); // cached head 0: 4 - 3 = 1, enough for wanted = 1
        assert_eq!(producer.free(2), 2); // reloads head: 1 released, 4 - 2 = 2
    }

    #[test]
    fn peek_does_not_consume() {
        let (mut producer, mut consumer) = channel::<2>(4);
        producer.write(&record(7, 16));
        producer.write(&record(8, 16));
        producer.publish();
        assert_eq!(consumer.available(1), 2);
        assert_eq!(consumer.peek(0), record(7, 16)[0]);
        assert_eq!(consumer.peek(15), record(7, 16)[15]);
        assert_eq!(consumer.peek(0), record(7, 16)[0], "peeking twice sees the same record");
        assert_eq!(read_one(&mut consumer, 16), record(7, 16));
        assert_eq!(consumer.peek(3), record(8, 16)[3], "after a read, peek sees the next record");
    }

    #[test]
    fn peek_at_sees_a_later_record_and_is_closed_says_when_nothing_more_comes() {
        let (mut producer, mut consumer) = channel::<1>(4);
        for i in 0..3 {
            producer.write(&record(i, 8));
        }
        producer.publish();
        assert_eq!(consumer.available(1), 3);
        assert_eq!(consumer.peek_at(2, 5), record(2, 8)[5], "the last record published");
        assert_eq!(consumer.peek_at(0, 1), consumer.peek(1));
        read_one(&mut consumer, 8);
        assert_eq!(consumer.peek_at(1, 0), record(2, 8)[0], "counted from the next unread record");
        assert!(!consumer.is_closed());
        drop(producer);
        assert!(consumer.is_closed() && !consumer.is_finished(), "closed, but two records are left");
    }

    #[test]
    #[should_panic(expected = "call available() first")]
    fn peek_at_past_what_is_available_panics() {
        let (mut producer, mut consumer) = channel::<1>(4);
        producer.write(&record(0, 8));
        producer.publish();
        assert_eq!(consumer.available(1), 1);
        consumer.peek_at(1, 0);
    }

    #[test]
    #[should_panic(expected = "call available() first")]
    fn peek_on_an_empty_ring_panics() {
        let (_producer, consumer) = channel::<1>(2);
        consumer.peek(0);
    }

    #[test]
    #[should_panic(expected = "check free() first")]
    fn writing_into_a_full_ring_panics() {
        let (mut producer, _consumer) = channel::<1>(1);
        producer.write(&[1]);
        producer.write(&[2]);
    }

    #[test]
    #[should_panic(expected = "doesn't fit")]
    fn a_record_longer_than_the_slot_panics() {
        let (mut producer, _consumer) = channel::<1>(1);
        producer.write(&[0; 9]);
    }

    #[test]
    #[should_panic(expected = "check available() first")]
    fn reading_an_empty_ring_panics() {
        let (_producer, mut consumer) = channel::<1>(1);
        consumer.read(&mut [0]);
    }

    #[test]
    fn a_shorter_record_leaves_the_rest_of_the_slot_alone() {
        let (mut producer, mut consumer) = channel::<1>(1);
        producer.write(&[1, 2, 3, 4, 5, 6, 7, 8]);
        producer.publish();
        read_one(&mut consumer, 8);
        consumer.release();
        producer.write(&[9, 10]);
        producer.publish();
        // Reading only what was written is the reader's job; the rest is the old record.
        assert_eq!(read_one(&mut consumer, 8), [9, 10, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn after_the_producer_is_dropped_the_ring_finishes_once_every_record_is_read() {
        let (mut producer, mut consumer) = channel::<1>(4);
        producer.write(&[1]);
        producer.publish();
        producer.write(&[2]); // written, never published: the drop publishes it
        assert!(!consumer.is_finished(), "still open");
        drop(producer);
        assert!(!consumer.is_finished(), "closed, but two records are left");
        assert_eq!(consumer.available(2), 2);
        assert_eq!(read_one(&mut consumer, 1), [1]);
        assert!(!consumer.is_finished(), "closed, one record left");
        assert_eq!(read_one(&mut consumer, 1), [2]);
        assert!(consumer.is_finished(), "closed and empty");
        assert!(consumer.is_finished(), "and it stays so");
    }

    #[test]
    fn a_ring_closed_empty_is_finished_at_once() {
        let (producer, mut consumer) = channel::<3>(2);
        drop(producer);
        assert!(consumer.is_finished());
        assert_eq!(consumer.available(1), 0);
    }

    #[test]
    fn a_new_ring_reads_as_zeros_after_pre_touching() {
        let (mut producer, mut consumer) = channel::<3>(2);
        producer.write(&[]);
        producer.publish();
        // An empty record was written: the slot still holds what `channel` left, zeros.
        assert_eq!(read_one(&mut consumer, 24), vec![0; 24]);
    }
}
