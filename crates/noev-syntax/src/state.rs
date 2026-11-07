//! The labels inside kets and bras (`spec/types.md` §3): `0 1 + -`, `(+i)` and `(-i)` for
//! the Y basis, each one qubit, and computed labels `{expr}`.

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

    /// The label of the standard library's `State` variant number `index`, which lists them
    /// in this order.
    pub fn from_index(index: u32) -> Option<Label> {
        Some(match index {
            0 => Label::Zero,
            1 => Label::One,
            2 => Label::Plus,
            3 => Label::Minus,
            4 => Label::PlusI,
            5 => Label::MinusI,
            _ => return None,
        })
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

/// The fixed label at the start of `bytes`, and its length in bytes.
pub fn label(bytes: &[u8]) -> Option<(Label, usize)> {
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

/// How many bytes of labels `bytes` starts with, computed ones (`{expr}`) included.
pub fn scan(bytes: &[u8]) -> usize {
    let mut end = 0;
    loop {
        if let Some((_, len)) = label(&bytes[end..]) {
            end += len;
        } else if bytes.get(end) == Some(&b'{')
            && let Ok(close) = crate::lexer::scan_interpolation(bytes, end)
        {
            end = close;
        } else {
            return end;
        }
    }
}

/// The fixed labels of `text`, the inside of a ket or bra, in order. Stops at anything else,
/// such as a computed label.
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
        assert_eq!(scan(b"0{bits[0]}{f(\"}\")}1>"), 19);
        assert_eq!(Label::from_index(4), Some(Label::PlusI));
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
