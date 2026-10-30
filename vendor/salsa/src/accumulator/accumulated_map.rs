use std::ops;

use rustc_hash::FxBuildHasher;

use crate::IngredientIndex;
use crate::accumulator::accumulated::Accumulated;
use crate::accumulator::{Accumulator, AnyAccumulated};

#[derive(Default)]
pub struct AccumulatedMap {
    // dyn: one box per accumulator kind a query pushes to, only for queries that accumulate
    map: hashbrown::HashMap<IngredientIndex, Box<dyn AnyAccumulated>, FxBuildHasher>,
}

impl std::fmt::Debug for AccumulatedMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccumulatedMap")
            .field("map", &self.map.keys())
            .finish()
    }
}

impl AccumulatedMap {
    pub fn accumulate<A: Accumulator>(&mut self, index: IngredientIndex, value: A) {
        self.map
            .entry(index)
            .or_insert_with(|| <Box<Accumulated<A>>>::default())
            .accumulate(value);
    }

    pub fn extend_with_accumulated<'slf, A: Accumulator>(
        &'slf self,
        index: IngredientIndex,
        output: &mut Vec<&'slf A>,
    ) {
        let Some(a) = self.map.get(&index) else {
            return;
        };

        a.as_dyn_any()
            .downcast_ref::<Accumulated<A>>()
            .unwrap()
            .extend_with_accumulated(output);
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear()
    }

    pub fn allocation_size(&self) -> usize {
        self.map.allocation_size()
    }
}

/// Tracks whether any input read during a query's execution has any accumulated values.
///
/// Knowning whether any input has accumulated values makes aggregating the accumulated values
/// cheaper because we can skip over entire subtrees.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum InputAccumulatedValues {
    /// The query nor any of its inputs have any accumulated values.
    #[default]
    Empty,

    /// The query or any of its inputs have at least one accumulated value.
    Any,
}

impl InputAccumulatedValues {
    pub const fn is_any(self) -> bool {
        matches!(self, Self::Any)
    }

    pub const fn is_empty(self) -> bool {
        matches!(self, Self::Empty)
    }

    pub fn or_else(self, other: impl FnOnce() -> Self) -> Self {
        if self.is_any() { Self::Any } else { other() }
    }
}

impl ops::BitOr for InputAccumulatedValues {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (Self::Any, _) | (_, Self::Any) => Self::Any,
            (Self::Empty, Self::Empty) => Self::Empty,
        }
    }
}

impl ops::BitOrAssign for InputAccumulatedValues {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}
