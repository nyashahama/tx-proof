use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Seed(u64);

impl Seed {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Decision<T> {
    selected: T,
    ordinal: u64,
    eligible_count: usize,
}

impl<T> Decision<T> {
    #[must_use]
    pub const fn decision_number(&self) -> u64 {
        self.ordinal
    }

    #[must_use]
    pub const fn eligible_count(&self) -> usize {
        self.eligible_count
    }

    #[must_use]
    pub const fn selected(&self) -> &T {
        &self.selected
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoEligibleActions;

pub struct DecisionEngine {
    rng: ChaCha20Rng,
    decision_number: u64,
}

impl DecisionEngine {
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        Self {
            rng: ChaCha20Rng::seed_from_u64(seed.0),
            decision_number: 0,
        }
    }

    /// Chooses one item from a stable ordering of the eligible set.
    ///
    /// # Errors
    ///
    /// Returns [`NoEligibleActions`] when the eligible set is empty. An empty
    /// call does not consume a decision from the deterministic stream.
    pub fn choose<T, I>(&mut self, eligible: I) -> Result<Decision<T>, NoEligibleActions>
    where
        T: Ord,
        I: IntoIterator<Item = T>,
    {
        let mut eligible = eligible.into_iter().collect::<Vec<_>>();
        eligible.sort_unstable();
        eligible.dedup();

        if eligible.is_empty() {
            return Err(NoEligibleActions);
        }

        let eligible_count = eligible.len();
        let selected_index = self.rng.random_range(0..eligible_count);
        let selected = eligible.swap_remove(selected_index);
        let decision = Decision {
            selected,
            ordinal: self.decision_number,
            eligible_count,
        };
        self.decision_number += 1;

        Ok(decision)
    }
}
