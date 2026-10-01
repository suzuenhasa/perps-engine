//! When each item is sent: the send schedule (`docs/PIPELINE.md` 14.9; `docs/DECISIONS.md`
//! D-027, and D-034 for bursts).
//!
//! **Contract.** For a phase's items and an offered rate `R` (client commands a second),
//! [`Schedule::send_times`] gives each item its send time, in nanoseconds from the phase's
//! start, computed before the run into one `Vec<u64>` (8 bytes an item):
//! - **Client items** arrive as a Poisson process at rate `R`: each gap is
//!   `round(−ln(1 − u) × 10^9 / R)` ns, with `u = (next_u64() >> 11) / 2^53`, a uniform
//!   number in `[0, 1)` from the `SCHEDULE` stream (14.6). So the offered rate counts client
//!   commands only (C38).
//! - **Operator items** take no time of their own: each is sent right after the client item
//!   before it (gap 0), or at the phase's start if none precedes it. So the client arrivals
//!   are exactly Poisson at `R` in aggregate (14.9). Setup phase A, all operator items, is
//!   therefore sent as fast as the operator ring takes it.
//! - Times never decrease, and the item order is the plan's: the schedule changes only
//!   *when* each item goes, never what is sent or in which order (14.1).
//!
//! **Why Poisson.** Real arrivals are random, and random arrivals queue; evenly spaced
//! arrivals hide that and flatter the tail. [`Arrivals::Uniform`] (every gap `10^9 / R`,
//! with no drift) exists for debugging and is never reported.
//!
//! **Bursts** ([`Arrivals::Cox`], `--bursts median|busiest`, D-034). Real traffic is not
//! Poisson at a constant rate: Polymarket's per-second message rate swings, within an hour,
//! from about half its mean to four times it. A Cox process reproduces that: a Poisson process
//! whose rate is `R × m(s)` in second `s` of the phase, with
//! - `m(s) ∝ exp(fast(s) + slow(s))`, two AR(1) processes on the log of the rate, stepped
//!   once a second: `x(s) = phi × x(s − 1) + N(0, innovation_sd²)`, each starting from a draw
//!   of its stationary distribution, `N(0, innovation_sd² / (1 − phi²))`. The fast one is
//!   the second-to-second noise, the slow one the drift over minutes. Their parameters are
//!   the profile's fits to the recorded per-second rate ([`Bursts`]).
//! - **Normalised over the phase**: the phase's `n` client items at `R` a second last
//!   `T = n / R` seconds on average, and its multipliers are divided by one number chosen so
//!   that their integral over `[0, T)` is exactly `T` (whole seconds, then the fraction of
//!   the last). So the phase offers `R` on average over exactly its own length, like a
//!   Poisson phase, and its last item goes at `T` give or take the Poisson noise: a run's
//!   window closes a tail before `T`, and the flow must not end before it. (Dividing by the
//!   mean of the first `ceil(T) + 1` whole seconds, as first built, normalised over up to 2 s
//!   more than the phase, which then ran out of items up to a second before `T`: at the
//!   headline's shape, for 44% of seeds with the median preset and 32% with the busiest.)
//!   The profile's long-run normalisation, `exp(V / 2)` (`V` the two stationary variances'
//!   sum), is the same thing over many hours; over one run the slow component barely moves
//!   (half-life 326 s in the median hour), and its level alone could put the run's load 20%
//!   away from `R`, which would change the load, not just its shape. The first `ceil(T) + 1`
//!   seconds are drawn with the phase; seconds past those, if the phase runs longer,
//!   continue both processes with the same divisor.
//! - An arrival spends a unit exponential draw, `−ln(1 − u)`, at rate `R × m(s)` through the
//!   seconds it crosses (the exact inhomogeneous Poisson process, since the rate is constant
//!   within a second). The normal draws are Box–Muller: `sqrt(−2 ln(1 − u1)) × cos(2π u2)`.
//! - Only the schedule changes, never the plan (14.1). Every multiplier and send time is
//!   computed here, before the run: the sender reads the same `Vec<u64>` as for Poisson
//!   arrivals, so bursts cost the send path nothing and allocate nothing there.
//!
//! **The floats of the generator.** `ln`, `exp` and `cos` can round differently on different
//! machines, so floats never decide what a flow contains; here they decide only when a
//! message is sent, which is not reproducible to the nanosecond anyway.
//!
//! **One stream across phases.** A run's phases B1, B2 and timed are scheduled from one
//! [`Schedule`] in order, each with its own rate, so each phase continues the stream where
//! the last one stopped. The same seed gives the same relative spacing at every rate. (Cox
//! arrivals draw each phase's multipliers from the stream first, at the phase's start.)
//!
//! **Complexity.** One draw and one `ln` per client item; Cox arrivals add, per phase, two
//! normal draws per second of the phase, and per client item a few float operations per
//! second boundary it crosses.

use crate::SplitMix64;
use crate::market_flow::profile::{Ar1, BurstModel, POLYMARKET};
use crate::market_flow::{Item, stream, stream_ids};

/// Nanoseconds in a second.
const SECOND_NS: f64 = 1e9;

/// How client items arrive (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrivals {
    /// Exponential gaps at the offered rate: what every reported run uses unless it asks for
    /// bursts.
    Poisson,
    /// Every gap exactly `10^9 / R`: for debugging only.
    Uniform,
    /// A Poisson process whose rate swings each second as recorded (module docs, "Bursts").
    Cox(Bursts),
}

/// Which recorded hour's burstiness `--bursts` reproduces (D-034; the profile's fits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bursts {
    /// The median hour: fast `phi` 0.358, innovation sd 0.364; slow 0.99788 and 0.0129.
    /// About 1.6 × `R` at the 90th percentile of seconds, 2.4 × at the 99th, 4 × at most.
    Median,
    /// The busiest hour, smoother: fast 0.251 and 0.218; slow 0.99913 and 0.0054. About
    /// 1.3 × `R` at the 90th percentile, 1.7 × at the 99th, 2.3 × at most.
    Busiest,
}

impl Bursts {
    /// The preset's two AR(1) processes, from the profile.
    pub fn model(self) -> BurstModel {
        match self {
            Bursts::Median => POLYMARKET.bursts_median,
            Bursts::Busiest => POLYMARKET.bursts_busiest,
        }
    }
}

/// A source of send times, continued across a run's phases (module docs).
#[derive(Clone, Debug)]
pub struct Schedule {
    arrivals: Arrivals,
    /// The `SCHEDULE` stream (14.6); `Uniform` draws nothing from it.
    stream: SplitMix64,
}

impl Schedule {
    pub fn new(arrivals: Arrivals, seed: u64) -> Schedule {
        Schedule { arrivals, stream: stream(seed, stream_ids::SCHEDULE) }
    }

    /// The send time of each of `items`, in ns from the phase's start, with client items at
    /// `rate` a second (module docs). Panics if `rate` is 0 while there are client items.
    pub fn send_times(&mut self, items: &[Item], rate: u64) -> Vec<u64> {
        let mut times = Vec::with_capacity(items.len());
        let mut now: u64 = 0;
        // Client items sent so far in this phase, for uniform arrivals.
        let mut clients: u64 = 0;
        // Cox arrivals: this phase's rate multipliers, drawn before its first arrival.
        let mut cox = match self.arrivals {
            Arrivals::Cox(bursts) if rate > 0 => {
                let phase_clients = items.iter().filter(|item| item.is_client()).count() as u64;
                Some(CoxRate::new(bursts.model(), phase_clients, rate, &mut self.stream))
            }
            _ => None,
        };
        for item in items {
            if item.is_client() {
                assert!(rate > 0, "a phase with client items needs a rate above zero");
                clients += 1;
                now = match self.arrivals {
                    Arrivals::Poisson => now + self.poisson_gap(rate),
                    // Each time is computed from the start, so rounding never accumulates.
                    Arrivals::Uniform => (u128::from(clients) * 1_000_000_000 / u128::from(rate)) as u64,
                    Arrivals::Cox(_) => {
                        let cox = cox.as_mut().expect("drawn above, since the rate is above zero");
                        cox.next_arrival(now, rate, &mut self.stream)
                    }
                };
            }
            times.push(now); // an operator item: the time of the client item before it
        }
        times
    }

    /// One exponential gap at `rate` a second: `round(−ln(1 − u) × 10^9 / rate)` ns.
    fn poisson_gap(&mut self, rate: u64) -> u64 {
        let u = uniform_unit(&mut self.stream);
        // `1 − u` is in (0, 1], so the logarithm is finite and the gap is never negative.
        (-(1.0 - u).ln() * SECOND_NS / rate as f64).round() as u64
    }
}

/// A uniform number in `[0, 1)` with 53 random bits, as `f64` can hold exactly:
/// `(next_u64() >> 11) / 2^53`.
fn uniform_unit(stream: &mut SplitMix64) -> f64 {
    (stream.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// A standard normal draw (Box–Muller): `sqrt(−2 ln(1 − u1)) × cos(2π u2)`.
fn standard_normal(stream: &mut SplitMix64) -> f64 {
    let (u1, u2) = (uniform_unit(stream), uniform_unit(stream));
    (-2.0 * (1.0 - u1).ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// One AR(1) log-rate process (module docs, "Bursts").
#[derive(Clone, Copy, Debug)]
struct Ar1State {
    params: Ar1,
    value: f64,
}

impl Ar1State {
    /// Starts at a draw of the stationary distribution, `N(0, innovation_sd² / (1 − phi²))`.
    fn stationary(params: Ar1, stream: &mut SplitMix64) -> Ar1State {
        let sd = params.innovation_sd / (1.0 - params.phi * params.phi).sqrt();
        Ar1State { params, value: sd * standard_normal(stream) }
    }

    /// The next second: `phi × x + N(0, innovation_sd²)`.
    fn step(&mut self, stream: &mut SplitMix64) {
        self.value = self.params.phi * self.value + self.params.innovation_sd * standard_normal(stream);
    }
}

/// The rate multipliers of one phase of Cox arrivals (module docs, "Bursts"): `m(s)`, second
/// by second, divided so that they average exactly 1 over the phase's expected length.
#[derive(Clone, Debug)]
struct CoxRate {
    fast: Ar1State,
    slow: Ar1State,
    /// `exp(fast + slow)` of each second drawn so far, not yet divided.
    raw: Vec<f64>,
    /// `raw`'s mean over the phase's expected length: every multiplier is divided by it.
    divisor: f64,
}

impl CoxRate {
    /// The multipliers of a phase of `clients` client items at `rate` a second, which last
    /// `T = clients / rate` seconds on average: draws the first `ceil(T) + 1` seconds, and
    /// divides them so that their integral over `[0, T)` is exactly `T` (module docs).
    fn new(model: BurstModel, clients: u64, rate: u64, stream: &mut SplitMix64) -> CoxRate {
        let fast = Ar1State::stationary(model.fast, stream);
        let slow = Ar1State::stationary(model.slow, stream);
        let mut cox = CoxRate { fast, slow, raw: vec![(fast.value + slow.value).exp()], divisor: 1.0 };
        while (cox.raw.len() as u64) < clients.div_ceil(rate) + 1 {
            cox.draw_next_second(stream);
        }
        // `raw` over [0, T): its whole seconds, then the drawn fraction of the next one.
        let (whole, fraction) = ((clients / rate) as usize, (clients % rate) as f64 / rate as f64);
        let integral = cox.raw[..whole].iter().sum::<f64>() + fraction * cox.raw[whole];
        let length = clients as f64 / rate as f64;
        // A phase without client items never uses its multipliers: any divisor will do.
        cox.divisor = if clients == 0 { cox.raw[0] } else { integral / length };
        cox
    }

    /// Steps both processes to the next second and records its `exp(fast + slow)`.
    fn draw_next_second(&mut self, stream: &mut SplitMix64) {
        self.fast.step(stream);
        self.slow.step(stream);
        self.raw.push((self.fast.value + self.slow.value).exp());
    }

    /// `m(second)`: past the seconds drawn so far, both processes continue.
    fn multiplier(&mut self, second: usize, stream: &mut SplitMix64) -> f64 {
        while self.raw.len() <= second {
            self.draw_next_second(stream);
        }
        self.raw[second] / self.divisor
    }

    /// The first arrival after `now` (ns from the phase's start) at `rate × m(s)`: a unit
    /// exponential draw, spent second by second (module docs).
    fn next_arrival(&mut self, now: u64, rate: u64, stream: &mut SplitMix64) -> u64 {
        // `1 − u` is in (0, 1], so the work is finite and never negative.
        let mut work = -(1.0 - uniform_unit(stream)).ln();
        let mut t = now;
        loop {
            let second = (t / 1_000_000_000) as usize;
            let end = (second as u64 + 1) * 1_000_000_000;
            // Arrivals per nanosecond in this second.
            let per_ns = rate as f64 * self.multiplier(second, stream) / SECOND_NS;
            let room = (end - t) as f64 * per_ns;
            if work <= room {
                return t + (work / per_ns).round() as u64;
            }
            work -= room;
            t = end;
        }
    }
}

/// The Poisson send times of `items` at `rate`, from a fresh `SCHEDULE` stream of `seed`
/// (19.2's API; a run schedules its phases with one [`Schedule`] instead).
pub fn poisson_schedule(items: &[Item], rate: u64, seed: u64) -> Vec<u64> {
    Schedule::new(Arrivals::Poisson, seed).send_times(items, rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_flow::ClientItem;
    use engine::command::{CancelOrder, Command, SetMark};

    fn client() -> Item {
        Item::Client(ClientItem {
            account: 1,
            nonce: 1,
            command: Command::CancelOrder(CancelOrder { order_id: 1, market: 1 }),
        })
    }

    fn operator() -> Item {
        Item::Operator(Command::SetMark(SetMark { price: 100, market: 1 }))
    }

    #[test]
    fn poisson_gaps_have_mean_one_over_the_rate_and_an_exponential_spread() {
        // 18.1: the mean gap is 1/R within 1% over 1M gaps.
        let rate = 100_000;
        let items = vec![client(); 1_000_000];
        let times = poisson_schedule(&items, rate, 1);
        let mean = *times.last().expect("times") as f64 / items.len() as f64;
        let expected = 1e9 / rate as f64; // 10,000 ns
        assert!((mean / expected - 1.0).abs() < 0.01, "mean gap {mean:.1} ns");
        // Exponential: P(gap > mean) = 1/e = 36.8%, and P(gap > 3 × mean) = 5.0%.
        let gaps: Vec<u64> = std::iter::once(times[0]).chain(times.windows(2).map(|w| w[1] - w[0])).collect();
        let above =
            |factor: f64| gaps.iter().filter(|&&gap| gap as f64 > factor * expected).count() as f64 / 1e6;
        assert!((above(1.0) - (-1f64).exp()).abs() < 0.005, "{:.4} above the mean", above(1.0));
        assert!((above(3.0) - (-3f64).exp()).abs() < 0.003, "{:.4} above 3 × the mean", above(3.0));
    }

    #[test]
    fn operator_items_go_right_after_the_client_item_before_them() {
        let items = [operator(), client(), operator(), operator(), client(), client(), operator()];
        let times = poisson_schedule(&items, 1_000, 3);
        assert_eq!(times[0], 0, "at the phase's start");
        assert_eq!((times[2], times[3]), (times[1], times[1]));
        assert_eq!(times[6], times[5]);
        assert!(times[1] > 0 && times[4] >= times[1] && times[5] >= times[4]);
        // Setup phase A: operator items only, all at once.
        assert_eq!(poisson_schedule(&[operator(); 5], 20_000, 1), [0; 5]);
    }

    #[test]
    fn the_same_seed_gives_the_same_schedule_and_the_rate_only_scales_it() {
        let items = vec![client(); 1_000];
        assert_eq!(poisson_schedule(&items, 20_000, 9), poisson_schedule(&items, 20_000, 9));
        assert_ne!(poisson_schedule(&items, 20_000, 9), poisson_schedule(&items, 20_000, 10));
        let slow = poisson_schedule(&items, 10_000, 9);
        let fast = poisson_schedule(&items, 20_000, 9);
        // The same draws, twice as fast (to within rounding, 1 ns per gap).
        for (s, f) in slow.iter().zip(&fast) {
            assert!(s.abs_diff(2 * f) <= 2_000, "{s} and {f}");
        }
    }

    #[test]
    fn a_schedule_continues_its_stream_across_phases() {
        let items = vec![client(); 100];
        let mut schedule = Schedule::new(Arrivals::Poisson, 5);
        let first = schedule.send_times(&items, 1_000);
        let second = schedule.send_times(&items, 1_000);
        assert_ne!(first, second, "the second phase draws new gaps");
        let both = poisson_schedule(&[items.clone(), items].concat(), 1_000, 5);
        assert_eq!(both[..100], first[..]);
        let offset = first[99];
        assert!(both[100..].iter().zip(&second).all(|(b, s)| *b == s + offset));
    }

    #[test]
    fn uniform_arrivals_are_evenly_spaced_without_drift() {
        let items = [client(), operator(), client(), client()];
        let times = Schedule::new(Arrivals::Uniform, 1).send_times(&items, 3);
        assert_eq!(times, [333_333_333, 333_333_333, 666_666_666, 1_000_000_000]);
    }

    /// The `q`-quantile of `values` (sorted here), for `q` in `[0, 1]`.
    fn quantile(values: &mut [f64], q: f64) -> f64 {
        values.sort_by(f64::total_cmp);
        values[((values.len() - 1) as f64 * q).round() as usize]
    }

    #[test]
    fn cox_multipliers_average_one_and_swing_as_the_recorded_hours_do() {
        // The profile's calibration (flow.md, section 3) simulated its fitted models hour by
        // hour and took the median over hours of the per-second rate over its mean, at the
        // 90th and 99th percentiles and at the maximum: 1.57 / 2.44 / 4.05 for the median
        // hour's model (1.66 / 2.53 / 3.98 recorded), 1.33 / 1.72 / 2.31 for the busiest
        // hour's (1.32 / 1.65 / 1.98 recorded). The same here, over 9 simulated hours: p90 and
        // p99 within 12%; the maximum, one second an hour, varies most (3.5 to 4.7 from hour to
        // hour in the calibration's median-hour simulations), so within 20%.
        for (bursts, expected) in
            [(Bursts::Median, [1.57, 2.44, 4.05]), (Bursts::Busiest, [1.33, 1.72, 2.31])]
        {
            let mut stream = SplitMix64::new(11);
            let mut per_hour: [Vec<f64>; 3] = Default::default();
            for _ in 0..9 {
                // An hour's phase: 3,600,000 items at 1,000 a second.
                let mut rate = CoxRate::new(bursts.model(), 3_600_000, 1_000, &mut stream);
                let mut hour: Vec<f64> = (0..3_600).map(|s| rate.multiplier(s, &mut stream)).collect();
                let mean = hour.iter().sum::<f64>() / 3_600.0;
                assert!((mean - 1.0).abs() < 1e-9, "{bursts:?}: normalised to a mean of 1, not {mean}");
                for (quantiles, q) in per_hour.iter_mut().zip([0.9, 0.99, 1.0]) {
                    quantiles.push(quantile(&mut hour, q));
                }
            }
            let medians = per_hour.map(|mut quantiles| quantile(&mut quantiles, 0.5));
            for ((actual, expected), (what, tolerance)) in
                medians.iter().zip(expected).zip([("p90", 0.12), ("p99", 0.12), ("max", 0.2)])
            {
                assert!(
                    (actual / expected - 1.0).abs() < tolerance,
                    "{bursts:?}: {what} {actual:.2}, not {expected}"
                );
            }
        }
    }

    #[test]
    fn cox_arrivals_follow_the_multipliers_and_keep_the_offered_rate() {
        let rate = 1_000;
        let items = vec![client(); 300_000]; // 300 s at 1,000 a second
        let times = Schedule::new(Arrivals::Cox(Bursts::Median), 4).send_times(&items, rate);
        assert!(times.windows(2).all(|pair| pair[0] <= pair[1]), "times never decrease");
        // The phase lasts about n / R: its multipliers average 1 over those seconds.
        let seconds = *times.last().expect("times") as f64 / 1e9;
        assert!((seconds / 300.0 - 1.0).abs() < 0.05, "300,000 arrivals at 1,000/s took {seconds:.1} s");
        // Each second's count is Poisson around R × m(s): the same multipliers, drawn again
        // from a fresh stream in the same order.
        let mut stream = stream(4, stream_ids::SCHEDULE);
        let mut cox = CoxRate::new(Bursts::Median.model(), 300_000, rate, &mut stream);
        let mut counts = vec![0u64; 400];
        times.iter().for_each(|&t| counts[(t / 1_000_000_000) as usize] += 1);
        let (mut expected, mut actual) = (0.0, 0.0);
        for (second, &count) in counts.iter().enumerate().take(290) {
            expected += rate as f64 * cox.multiplier(second, &mut stream);
            actual += count as f64;
        }
        assert!((actual / expected - 1.0).abs() < 0.01, "{actual} arrivals against {expected:.0} expected");
        // And they are bursty: the busiest second holds well over twice the mean.
        assert!(counts.iter().take(290).any(|&count| count > 2 * rate), "{counts:?}");
        // The same seed gives the same schedule; another seed another one.
        assert_eq!(times, Schedule::new(Arrivals::Cox(Bursts::Median), 4).send_times(&items, rate));
        assert_ne!(times, Schedule::new(Arrivals::Cox(Bursts::Median), 5).send_times(&items, rate));
    }

    #[test]
    fn a_cox_phase_offers_exactly_its_length_and_ends_on_time() {
        // The headline's timed phase: a 5 s warm-up, a 60 s window and a 0.2 s tail at
        // 100,000 a second, 6,520,000 items over 65.2 s. For every seed, the multipliers'
        // integral over [0, 65.2 s) is 65.2 s, so the phase offers 6,520,000 arrivals on
        // average by 65.2 s, never ending a second early.
        let (clients, rate) = (6_520_000, 100_000);
        for bursts in [Bursts::Median, Bursts::Busiest] {
            for seed in 1..=20 {
                let mut stream = SplitMix64::new(seed);
                let mut cox = CoxRate::new(bursts.model(), clients, rate, &mut stream);
                let whole: f64 = (0..65).map(|second| cox.multiplier(second, &mut stream)).sum();
                let integral = whole + 0.2 * cox.multiplier(65, &mut stream);
                assert!((integral - 65.2).abs() < 1e-9, "{bursts:?}, seed {seed}: {integral}");
            }
        }
        // And a whole phase's last send time is its length, give or take the Poisson noise:
        // the smoke run's 30,000 items at 20,000 a second (1.5 s; its window closes at 1.3 s).
        let items = vec![client(); 30_000];
        for seed in 1..=8 {
            let times = Schedule::new(Arrivals::Cox(Bursts::Median), seed).send_times(&items, 20_000);
            let last = *times.last().expect("times");
            assert!(last.abs_diff(1_500_000_000) < 100_000_000, "seed {seed}: the last item at {last} ns");
        }
    }

    #[test]
    fn cox_arrivals_keep_operator_items_after_the_client_item_before_them() {
        let items = [operator(), client(), operator(), client(), client(), operator()];
        let times = Schedule::new(Arrivals::Cox(Bursts::Busiest), 3).send_times(&items, 1_000);
        assert_eq!((times[0], times[2], times[5]), (0, times[1], times[4]));
        assert!(times[1] > 0 && times[3] >= times[1] && times[4] >= times[3]);
        // Poisson arrivals are unchanged by the new mode: the same stream, the same gaps.
        assert_eq!(
            Schedule::new(Arrivals::Poisson, 3).send_times(&items, 1_000),
            poisson_schedule(&items, 1_000, 3)
        );
    }
}
