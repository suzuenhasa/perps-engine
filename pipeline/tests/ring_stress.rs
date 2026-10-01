//! Concurrency tests of the ring and the durable watermark (`docs/PIPELINE.md` 18.3).
//!
//! **Ring stress.** A producer thread writes records whose every word is a function of the
//! record's number, in random batch sizes, while the consumer reads in random batch sizes
//! and checks every word. So any record that is lost, repeated, reordered, torn (words from
//! two different records) or stale (a word from the slot's previous lap) fails the test.
//! It runs for 1-, 2- and 3-line slots, and once with a consumer much slower than the
//! producer on a small ring, so the full-ring paths run constantly. At the end the producer
//! is dropped, and the consumer must read every record before `is_finished` says true: it
//! stops only when `is_finished` is true, then checks that it read them all.
//!
//! Records: 10 million per slot size in release builds, as the spec asks; 1 million in
//! debug builds, where our crate is unoptimised and 10 million would take minutes. Run
//! `./dev cargo test -p pipeline --release --test ring_stress` for the full count.
//!
//! **Watermark.** A writer thread "flushes" (marks seqs durable) and then publishes an
//! increasing watermark, while a reader checks that the watermark never decreases, never
//! passes the writer's last store, and never covers a seq whose flush it can't see.
//!
//! **What these tests can and can't show.** They check the ring's logic: indexing,
//! wrap-around, the cached counters, full and empty, and closing. They can't check the
//! memory orderings (PIPELINE.md 3.2 and 11.7) on the machines they run on: x86 is strongly
//! ordered (it never reorders a store with a store or a load with a load), and there a
//! Relaxed and a Release/Acquire access compile to the same plain `mov`, so a Release weakened
//! to Relaxed would still pass here, although it would be wrong under Rust's memory model and
//! on ARM. The orderings rest on the written argument of 3.2, checked by review; only a weakly
//! ordered machine, or a model checker such as loom or Miri (new dependencies, which this
//! project doesn't take), would test them (review finding F-TEST-X86-ORDERING).
//!
//! Both sides idle with "spin then yield", so these tests share CPUs politely with the
//! other tests running at the same time. The outcome doesn't depend on the scheduling:
//! only how long they take does.

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use pipeline::counters::Watermark;
use pipeline::idle::IdleStrategy;
use pipeline::ring::{Consumer, Producer, WORDS_PER_LINE, channel};

const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 64 };

/// Records per stress run (module docs).
const RECORDS: u64 = if cfg!(debug_assertions) { 1_000_000 } else { 10_000_000 };

/// Word `w` of record `i`: different for every record and every word, so a torn or stale
/// record shows. Record `i` and record `i + capacity` share a slot and differ in every word.
fn word(i: u64, w: usize) -> u64 {
    (i << 8) | w as u64
}

/// A tiny deterministic generator (xorshift64) for the batch sizes.
struct XorShift(u64);

impl XorShift {
    /// A number in `1..=max`.
    fn one_to(&mut self, max: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        1 + (self.0 % max as u64) as usize
    }
}

/// One stress run's settings.
#[derive(Clone, Copy)]
struct Stress {
    records: u64,
    capacity: usize,
    /// Batches are random in `1..=max_batch` on both sides.
    max_batch: usize,
    /// Pause after every record the consumer reads, in `spin_loop` hints.
    consumer_delay: u32,
}

/// What each side saw, for the assertions.
#[derive(Debug)]
struct Outcome {
    read: u64,
    /// Producer passes that found no free slot.
    producer_full_passes: u64,
}

fn stress<const LINES: usize>(settings: Stress) -> Outcome {
    let (producer, consumer) = channel::<LINES>(settings.capacity);
    let writer = thread::spawn(move || produce(producer, settings));
    let read = consume(consumer, settings);
    let producer_full_passes = writer.join().expect("the producer ends cleanly");
    Outcome { read, producer_full_passes }
}

/// Writes every record, then drops the producer, which closes the ring.
fn produce<const LINES: usize>(mut producer: Producer<LINES>, settings: Stress) -> u64 {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut record = vec![0; WORDS_PER_LINE * LINES];
    let mut full_passes = 0;
    let mut next = 0;
    while next < settings.records {
        let wanted = rng.one_to(settings.max_batch).min((settings.records - next) as usize);
        let n = producer.free(wanted).min(wanted);
        if n == 0 {
            full_passes += 1;
            IDLE.idle();
            continue;
        }
        for _ in 0..n {
            for (w, slot_word) in record.iter_mut().enumerate() {
                *slot_word = word(next, w);
            }
            producer.write(&record);
            next += 1;
        }
        producer.publish();
    }
    full_passes
}

/// Reads until the ring is closed and empty, checking every word; returns the count read.
fn consume<const LINES: usize>(mut consumer: Consumer<LINES>, settings: Stress) -> u64 {
    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    let words = WORDS_PER_LINE * LINES;
    let mut record = vec![0; words];
    let mut next = 0;
    loop {
        let wanted = rng.one_to(settings.max_batch);
        let n = consumer.available(wanted).min(wanted);
        if n == 0 {
            if consumer.is_finished() {
                break;
            }
            IDLE.idle();
            continue;
        }
        for _ in 0..n {
            // Peeking shows the same record that is then read.
            assert_eq!(consumer.peek(words - 1), word(next, words - 1), "peek at record {next}");
            consumer.read(&mut record);
            for (w, &got) in record.iter().enumerate() {
                assert_eq!(got, word(next, w), "record {next}, word {w}");
            }
            next += 1;
            for _ in 0..settings.consumer_delay {
                std::hint::spin_loop();
            }
        }
        consumer.release();
    }
    next
}

fn fast(records: u64) -> Stress {
    Stress { records, capacity: 1_024, max_batch: 300, consumer_delay: 0 }
}

#[test]
fn one_line_records_arrive_intact_and_in_order() {
    let outcome = stress::<1>(fast(RECORDS));
    assert_eq!(outcome.read, RECORDS);
}

#[test]
fn two_line_records_arrive_intact_and_in_order() {
    let outcome = stress::<2>(fast(RECORDS));
    assert_eq!(outcome.read, RECORDS);
}

#[test]
fn three_line_records_arrive_intact_and_in_order() {
    let outcome = stress::<3>(fast(RECORDS));
    assert_eq!(outcome.read, RECORDS);
}

#[test]
fn a_slow_consumer_keeps_the_ring_full_and_still_gets_everything() {
    // A 16-slot ring, a consumer that pauses after every record, batches up to twice the
    // ring: the producer finds the ring full on most passes.
    let records = RECORDS / 50;
    let outcome = stress::<3>(Stress { records, capacity: 16, max_batch: 32, consumer_delay: 200 });
    assert_eq!(outcome.read, records);
    assert!(outcome.producer_full_passes > 0, "the full-ring path never ran: {outcome:?}");
}

#[test]
fn a_ring_of_one_slot_passes_records_one_at_a_time() {
    let records = RECORDS / 10;
    let outcome = stress::<2>(Stress { records, capacity: 1, max_batch: 3, consumer_delay: 0 });
    assert_eq!(outcome.read, records);
}

#[test]
fn the_watermark_never_decreases_and_never_passes_the_writers_last_store() {
    let last: u64 = RECORDS;
    let watermark = Watermark::new(0);
    // The "disk": `flushed[s]` becomes 1 when seq `s` is written, before it is published.
    let flushed: Vec<AtomicU64> = (0..=last).map(|_| AtomicU64::new(0)).collect();
    // The writer's last store, recorded before it publishes the watermark.
    let last_store = AtomicU64::new(0);
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut rng = XorShift(0x1234_5678_9ABC_DEF1);
            let mut durable = 0;
            while durable < last {
                let batch_end = (durable + rng.one_to(100) as u64).min(last);
                for seq in durable + 1..=batch_end {
                    flushed[seq as usize].store(1, Ordering::Relaxed);
                }
                last_store.store(batch_end, Ordering::Relaxed);
                watermark.publish(batch_end);
                durable = batch_end;
            }
        });
        let mut seen = 0;
        while seen < last {
            let w = watermark.load();
            assert!(w >= seen, "the watermark moved back from {seen} to {w}");
            assert!(
                w <= last_store.load(Ordering::Relaxed),
                "the watermark {w} passed the writer's last store"
            );
            for seq in seen + 1..=w {
                assert_eq!(
                    flushed[seq as usize].load(Ordering::Relaxed),
                    1,
                    "seq {seq} is covered but not flushed"
                );
            }
            if w == seen {
                IDLE.idle();
            }
            seen = w;
        }
    });
}
