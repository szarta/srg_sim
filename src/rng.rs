//! Seeded RNG — canonical **splitmix64** (DESIGN.md §5/§6).
//!
//! Every non-deterministic step in the engine goes through one [`SeededRNG`] so
//! a game is fully reproducible from its `header.seed` (DESIGN.md §8 replay).
//! The generator is **not** a language-specific PRNG but canonical splitmix64,
//! so the exact draw stream is identical in the Python oracle and here — across
//! native and `wasm32` targets alike. That is the prerequisite for byte-identical
//! logs (`docs/design/substrate-split.rst` §5).
//!
//! The whole contract is small enough to restate, and is matched bit-for-bit by
//! the Python reference (`fixtures/rng/splitmix64.json` pins it cross-language):
//!
//! * **state** — a single `u64`; the **seed** sets it directly.
//! * [`next_u64`](SeededRNG::next_u64) — add the golden-ratio increment
//!   `0x9E3779B97F4A7C15`, apply the two splitmix64 mixing steps, return the 64-bit
//!   result. All arithmetic is wrapping (`mod 2^64`).
//! * [`roll`](SeededRNG::roll) — `SKILL_FACES[next() % 6]`.
//! * [`shuffle`](SeededRNG::shuffle) — downward Fisher–Yates: for `i` from `n-1`
//!   down to `1`, swap `i` with `next() % (i + 1)`.
//! * [`reveal`](SeededRNG::reveal) — `items[next() % len]`.
//! * [`randint`](SeededRNG::randint) — `low + next() % (high - low + 1)`
//!   (inclusive).
//!
//! Modulo bias over `2^64` is negligible and, crucially, *identical* in both
//! engines, so a fixed `% n` rule keeps the two streams in lockstep.

use crate::ir::Skill;
use serde::{Deserialize, Serialize};

/// The six die faces, in the fixed contract order ([`Skill::ALL`]).
pub const SKILL_FACES: [Skill; 6] = Skill::ALL;

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15; // splitmix64 increment (golden ratio)
const MIX1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX2: u64 = 0x94D0_49BB_1331_11EB;

/// A canonical splitmix64 generator exposing the engine's roll/shuffle/reveal
/// primitives, snapshottable for bit-exact mid-game resume.
#[derive(Debug, Clone)]
pub struct SeededRNG {
    seed: u64,
    state: u64,
}

impl SeededRNG {
    /// Construct from an integer seed. The seed initializes the 64-bit state and
    /// is retained for the log header.
    pub fn new(seed: u64) -> Self {
        Self { seed, state: seed }
    }

    /// The seed this generator was constructed with (for the log header).
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Advance and return the next 64-bit output (canonical splitmix64).
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(MIX1);
        z = (z ^ (z >> 27)).wrapping_mul(MIX2);
        z ^ (z >> 31)
    }

    /// Return one of the six skill faces, uniformly (DESIGN.md §6).
    pub fn roll(&mut self) -> Skill {
        SKILL_FACES[(self.next_u64() % 6) as usize]
    }

    /// Shuffle `items` in place via downward Fisher–Yates (deterministic for the
    /// current state).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }

    /// Return one element of `items` uniformly (random discard / reveal), or
    /// `None` if the slice is empty.
    pub fn reveal<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        let idx = (self.next_u64() % items.len() as u64) as usize;
        Some(&items[idx])
    }

    /// A uniform integer in `[low, high]` (inclusive), for misc. effects.
    ///
    /// Panics if `high < low` (an empty range), mirroring the Python contract's
    /// assumption of a well-formed inclusive range.
    pub fn randint(&mut self, low: i64, high: i64) -> i64 {
        let span = (high - low + 1) as u64;
        low + (self.next_u64() % span) as i64
    }

    /// Serialize the generator state (JSON-friendly, see DESIGN.md §5).
    pub fn snapshot(&self) -> RngSnapshot {
        RngSnapshot {
            seed: self.seed,
            state: self.state,
        }
    }

    /// Rebuild a generator from [`snapshot`](SeededRNG::snapshot) output
    /// (bit-exact resume).
    pub fn restore(snapshot: &RngSnapshot) -> Self {
        Self {
            seed: snapshot.seed,
            state: snapshot.state,
        }
    }
}

/// The full generator state — a seed plus the single 64-bit word — captured for
/// mid-game serialization and bit-exact resume (DESIGN.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RngSnapshot {
    pub seed: u64,
    pub state: u64,
}

// A `SeededRNG` serializes *as* its snapshot (`{"seed", "state"}`), so a
// `GameState` embedding one round-trips exactly like the Python `rng.snapshot()`
// form (DESIGN.md §5).
impl Serialize for SeededRNG {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.snapshot().serialize(s)
    }
}

impl<'de> Deserialize<'de> for SeededRNG {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        RngSnapshot::deserialize(d).map(|snap| SeededRNG::restore(&snap))
    }
}
