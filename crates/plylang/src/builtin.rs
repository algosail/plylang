//! Compiler builtins. Checking, lowering, hover, and completion read this table.
//!
//! Value operations live in the prelude. The compiler keeps the stack, quotations,
//! and the word `Void`. `Void` is also a nullary constructor; the checker installs
//! that data type while loading the table. A digit, `-`, `*`, or `@` cannot start
//! one identifier.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lowering {
    /// Pop `take` values, bottom to top, and push `put` indexes. `0` is the deepest.
    Shuffle { take: u8, put: &'static [u8] },
    Call,
    /// Hide the top `under` values, run the quotation, then restore them.
    Dip { under: u8 },
    /// Run the quotation on the top `keep` values, then push those values again.
    Keep { keep: u8 },
    /// Apply each quotation to the same `vals` values. Each returns one value.
    Cleave { vals: u8, quots: u8 },
    /// Apply quotation `i` to value `i`. The deepest value is first.
    Spread { n: u8 },
    /// Apply one quotation to each of `n` values of the same type.
    Each { n: u8 },
    If,
    Quote,
    Unit,
}

pub struct Word {
    pub name: &'static str,
    pub scheme: &'static str,
    /// Prose under the scheme in hover and beside the name in completion.
    pub doc: &'static str,
    pub lowering: Lowering,
}

pub struct Type {
    pub name: &'static str,
    pub arity: usize,
}

const fn w(
    name: &'static str,
    scheme: &'static str,
    doc: &'static str,
    lowering: Lowering,
) -> Word {
    Word {
        name,
        scheme,
        doc,
        lowering,
    }
}

pub const TYPES: &[Type] = &[
    Type { name: "Int", arity: 0 },
    Type { name: "Num", arity: 0 },
    Type { name: "Str", arity: 0 },
    Type { name: "Bool", arity: 0 },
    Type { name: "Void", arity: 0 },
    Type { name: "Array", arity: 1 },
    Type { name: "StrMap", arity: 1 },
];

pub const WORDS: &[Word] = &[
    w(
        "dup",
        "..s a -- ..s a a",
        "Copies the top value. On `x y` the result is `x y y`.",
        shuffle(1, &[0, 0]),
    ),
    w(
        "drop",
        "..s a -- ..s",
        "Drops the top value. On `x y` the result is `x`.",
        shuffle(1, &[]),
    ),
    w(
        "swap",
        "..s a b -- ..s b a",
        "Swaps the top two values. On `x y` the result is `y x`.",
        shuffle(2, &[1, 0]),
    ),
    w(
        "nip",
        "..s a b -- ..s b",
        "Drops the value under the top. On `x y` the result is `y`.",
        shuffle(2, &[1]),
    ),
    w(
        "over",
        "..s a b -- ..s a b a",
        "Copies the value under the top onto the top. On `x y` the result is `x y x`.",
        shuffle(2, &[0, 1, 0]),
    ),
    w(
        "dupd",
        "..s a b -- ..s a a b",
        "Copies the value under the top. On `x y` the result is `x x y`.",
        shuffle(2, &[0, 0, 1]),
    ),
    w(
        "swapd",
        "..s a b c -- ..s b a c",
        "Swaps the two values under the top. On `x y z` the result is `y x z`.",
        shuffle(3, &[1, 0, 2]),
    ),
    w(
        "tuck",
        "..s a b -- ..s b a b",
        "Copies the top value under the one below it. On `x y` the result is `y x y`.",
        shuffle(2, &[1, 0, 1]),
    ),
    w(
        "pick",
        "..s a b c -- ..s a b c a",
        "Copies the third value onto the top. On `x y z` the result is `x y z x`.",
        shuffle(3, &[0, 1, 2, 0]),
    ),
    w(
        "rot",
        "..s a b c -- ..s b c a",
        "Moves the third value to the top. On `x y z` the result is `y z x`.",
        shuffle(3, &[1, 2, 0]),
    ),
    w(
        "unrot",
        "..s a b c -- ..s c a b",
        "Moves the top value under the next two. On `x y z` the result is `z x y`.",
        shuffle(3, &[2, 0, 1]),
    ),
    w(
        "dup2",
        "..s a b -- ..s a b a b",
        "Copies the top two values. On `x y` the result is `x y x y`.",
        shuffle(2, &[0, 1, 0, 1]),
    ),
    w(
        "drop2",
        "..s a b -- ..s",
        "Drops the top two values.",
        shuffle(2, &[]),
    ),
    w(
        "nip2",
        "..s a b c -- ..s c",
        "Drops the two values under the top. On `x y z` the result is `z`.",
        shuffle(3, &[2]),
    ),
    w(
        "swap2",
        "..s a b c d -- ..s c d a b",
        "Swaps the top two pairs. On `a b c d` the result is `c d a b`.",
        shuffle(4, &[2, 3, 0, 1]),
    ),
    w(
        "over2",
        "..s a b c d -- ..s a b c d a b",
        "Copies the pair under the top pair onto the top. On `a b c d` the result is `a b c d a b`.",
        shuffle(4, &[0, 1, 2, 3, 0, 1]),
    ),
    w(
        "dip",
        "..s a (..s -- ..t) -- ..t a",
        "Hides the value under the quotation, runs the quotation, and restores that value on top. The quotation is already on top.",
        Lowering::Dip { under: 1 },
    ),
    w(
        "dip2",
        "..s a b (..s -- ..t) -- ..t a b",
        "Hides two values under the quotation and restores them in the same order.",
        Lowering::Dip { under: 2 },
    ),
    w(
        "dip3",
        "..s a b c (..s -- ..t) -- ..t a b c",
        "Hides three values under the quotation and restores them in the same order.",
        Lowering::Dip { under: 3 },
    ),
    w(
        "keep",
        "..s a (..s a -- ..t) -- ..t a",
        "Runs the quotation, then pushes back the value it consumed. `5 [ 1n iadd ] keep` leaves the sum and the original five.",
        Lowering::Keep { keep: 1 },
    ),
    w(
        "keep2",
        "..s a b (..s a b -- ..t) -- ..t a b",
        "Runs the quotation, then pushes back the two values it consumed.",
        Lowering::Keep { keep: 2 },
    ),
    w(
        "bi",
        "..s a (a -- b) (a -- c) -- ..s b c",
        "Applies two quotations to one value. Each quotation returns one value. `5 [ 2 nmul ] [ 3 nadd ] bi` leaves `10`, then `8`.",
        Lowering::Cleave { vals: 1, quots: 2 },
    ),
    w(
        "tri",
        "..s a (a -- b) (a -- c) (a -- d) -- ..s b c d",
        "Applies three quotations to one value. Each quotation returns one value.",
        Lowering::Cleave { vals: 1, quots: 3 },
    ),
    w(
        "bis",
        "..s a b (a -- c) (b -- d) -- ..s c d",
        "The deeper quotation takes the deeper value, and the top quotation takes the top value. Each returns one value.",
        Lowering::Spread { n: 2 },
    ),
    w(
        "bia",
        "..s a a (a -- b) -- ..s b b",
        "Applies one quotation to each of two values of the same type.",
        Lowering::Each { n: 2 },
    ),
    w(
        "call",
        "..s (..s -- ..t) -- ..t",
        "Pops the quotation and runs it on the rest of the stack.",
        Lowering::Call,
    ),
    w(
        "quote",
        "..s a -- ..s (-- a)",
        "Wraps the top value in a quotation that pushes it again. `5 quote` has type `(-- Num)`.",
        Lowering::Quote,
    ),
    w(
        "if",
        "..s Bool (..s -- ..t) (..s -- ..t) -- ..t",
        "Pops a `Bool` and two quotations. The `true` branch is pushed first, so the `false` branch is on top.",
        Lowering::If,
    ),
    w(
        "Void",
        "..s -- ..s Void",
        "The only value of type `Void`.",
        Lowering::Unit,
    ),
];

const fn shuffle(take: u8, put: &'static [u8]) -> Lowering {
    Lowering::Shuffle { take, put }
}

pub fn word(name: &str) -> Option<&'static Word> {
    WORDS.iter().find(|word| word.name == name)
}

pub fn builtin_type(name: &str) -> bool {
    TYPES.iter().any(|ty| ty.name == name)
}

#[cfg(test)]
mod tests {
    use super::Lowering;
    use super::TYPES;
    use super::WORDS;

    #[test]
    fn names_are_unique() {
        let mut names = std::collections::BTreeSet::new();
        for word in WORDS {
            assert!(names.insert(word.name), "duplicate builtin {}", word.name);
            assert!(!word.doc.is_empty(), "builtin {} has no doc", word.name);
            if let Lowering::Shuffle { take, put } = word.lowering {
                for index in put {
                    assert!(
                        *index < take,
                        "{} puts index {index} of {take}",
                        word.name
                    );
                }
            }
        }
        let mut types = std::collections::BTreeSet::new();
        for ty in TYPES {
            assert!(types.insert(ty.name), "duplicate builtin type {}", ty.name);
        }
    }
}
