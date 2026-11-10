//! Monotonic counters used by the command/event contract.
//!
//! `Revision` versions entity state, `AttemptGeneration` fences retries of one
//! task, and `Sequence` orders events only within one operation/subscription
//! stream. None of these is a global clock.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! counter_type {
    ($name:ident, $primitive:ty) => {
        #[derive(
            Debug,
            Default,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name($primitive);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(value: $primitive) -> Self {
                Self(value)
            }

            pub const fn get(self) -> $primitive {
                self.0
            }

            pub fn checked_next(self) -> Option<Self> {
                self.0.checked_add(1).map(Self)
            }
        }

        impl From<$primitive> for $name {
            fn from(value: $primitive) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

counter_type!(Revision, u64);
counter_type!(Sequence, u64);
counter_type!(AttemptGeneration, u32);

impl AttemptGeneration {
    /// First attempt of a task. Zero is not a valid generation; task validation
    /// rejects it so that stale, default-initialized values cannot be accepted.
    pub const INITIAL: Self = Self(1);

    pub const fn is_initial(self) -> bool {
        self.0 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_do_not_wrap_silently() {
        assert_eq!(Sequence::new(u64::MAX).checked_next(), None);
        assert_eq!(AttemptGeneration::new(u32::MAX).checked_next(), None);
        assert_eq!(Revision::new(4).checked_next(), Some(Revision::new(5)));
    }

    #[test]
    fn counters_serialize_as_numbers() {
        assert_eq!(
            serde_json::to_string(&AttemptGeneration::INITIAL).expect("serialize"),
            "1"
        );
    }
}
