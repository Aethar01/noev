//! Interned identifiers.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// An interned identifier. Comparing symbols compares names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(u32);

/// Maps identifier text to [`Symbol`]s and back.
#[derive(Debug, Default)]
pub struct Interner {
    map: HashMap<Box<str>, Symbol, BuildHasherDefault<FxHasher>>,
    names: Vec<Box<str>>,
}

impl Interner {
    /// The symbol for `name`, creating it if this is the first time it is seen.
    pub fn intern(&mut self, name: &str) -> Symbol {
        if let Some(&symbol) = self.map.get(name) {
            return symbol;
        }
        let symbol = Symbol(u32::try_from(self.names.len()).expect("fewer than 2³² names"));
        self.names.push(name.into());
        self.map.insert(name.into(), symbol);
        symbol
    }

    /// The symbol for `name`, if it has been interned.
    pub fn get(&self, name: &str) -> Option<Symbol> {
        self.map.get(name).copied()
    }

    /// The text of `symbol`.
    pub fn name(&self, symbol: Symbol) -> &str {
        &self.names[symbol.0 as usize]
    }
}

/// The hash function rustc uses for its own tables: much faster than the standard library's
/// default on short keys, and not resistant to collision attacks, which a compiler does not
/// need.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        for &byte in rest {
            self.add(u64::from(byte));
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable() {
        let mut interner = Interner::default();
        let a = interner.intern("angle");
        let b = interner.intern("state");
        assert_eq!(interner.intern("angle"), a);
        assert_ne!(a, b);
        assert_eq!(interner.name(b), "state");
        assert_eq!(interner.get("missing"), None);
    }
}
