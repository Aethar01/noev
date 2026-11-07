//! The labels inside kets and bras (`spec/types.md` §3): `0 1 + -`, and `(+i)` and `(-i)`
//! for the Y basis. Each label is one qubit.

/// One qubit's state in a ket or bra.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    /// `0`: \|0⟩.
    Zero,
    /// `1`: \|1⟩.
    One,
    /// `+`: \|+⟩.
    Plus,
    /// `-`: \|−⟩.
    Minus,
    /// `(+i)`: \|+i⟩.
    PlusI,
    /// `(-i)`: \|−i⟩.
    MinusI,
}

/// The Pauli basis a label belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pauli {
    /// `+` and `-`.
    X,
    /// `(+i)` and `(-i)`.
    Y,
    /// `0` and `1`.
    Z,
}

impl Label {
    /// The basis whose eigenstate it is.
    pub fn basis(self) -> Pauli {
        match self {
            Label::Zero | Label::One => Pauli::Z,
            Label::Plus | Label::Minus => Pauli::X,
            Label::PlusI | Label::MinusI => Pauli::Y,
        }
    }

    /// Whether it is the −1 eigenstate: measuring in its basis gives 1.
    pub fn outcome(self) -> bool {
        matches!(self, Label::One | Label::Minus | Label::MinusI)
    }

    /// The state, as physics writes it: `|+i⟩`.
    pub fn ket(self) -> &'static str {
        match self {
            Label::Zero => "|0⟩",
            Label::One => "|1⟩",
            Label::Plus => "|+⟩",
            Label::Minus => "|−⟩",
            Label::PlusI => "|+i⟩",
            Label::MinusI => "|−i⟩",
        }
    }
}

/// The label at the start of `bytes`, and its length in bytes.
fn label(bytes: &[u8]) -> Option<(Label, usize)> {
    Some(match bytes {
        [b'0', ..] => (Label::Zero, 1),
        [b'1', ..] => (Label::One, 1),
        [b'(', b'+', b'i', b')', ..] => (Label::PlusI, 4),
        [b'(', b'-', b'i', b')', ..] => (Label::MinusI, 4),
        [b'+', ..] => (Label::Plus, 1),
        [b'-', ..] => (Label::Minus, 1),
        _ => return None,
    })
}

/// How many bytes of labels `bytes` starts with.
pub fn scan(bytes: &[u8]) -> usize {
    let mut end = 0;
    while let Some((_, len)) = label(&bytes[end..]) {
        end += len;
    }
    end
}

/// The labels of `text`, the inside of a ket or bra, in order. Stops at anything else.
pub fn labels(text: &str) -> impl Iterator<Item = Label> + '_ {
    let mut rest = text.as_bytes();
    std::iter::from_fn(move || {
        let (label, len) = label(rest)?;
        rest = &rest[len..];
        Some(label)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_lengths() {
        assert_eq!(scan(b"0(+i)-1>"), 7);
        assert_eq!(scan(b"(+j)>"), 0);
        let all: Vec<Label> = labels("01+-(+i)(-i)").collect();
        assert_eq!(
            all,
            [
                Label::Zero,
                Label::One,
                Label::Plus,
                Label::Minus,
                Label::PlusI,
                Label::MinusI
            ]
        );
        assert!(Label::MinusI.outcome() && Label::MinusI.basis() == Pauli::Y);
    }
}
