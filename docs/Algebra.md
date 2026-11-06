# Algebra

These are the equations of the [stack words](Words.md). Two sides of an equation lower to the same JavaScript. The words on the left are the program. An equation is not itself a word.

A shuffle reuses each value already on the stack. That value runs once, and the earlier value runs before the later one. A quotation written at the use is part of the same equation: `call`, `dip`, `dip2`, `dip3`, `keep`, `keep2`, `bi`, `tri`, `bis`, `bia`, and `if` run its body in place. `cons` substitutes the captured value into a quotation that was written there.

A word defined in the program stays a call. Its own body is simplified, and the call sites stay. A quotation that arrives as a value stays a call too.

An empty right-hand side means the inputs, in the order they arrived. One input is returned as itself. Several inputs are returned in that order.

## Identity

```text
swap swap          =
dup drop           =
quote call         =
[ ] call           =
[ ] dip            =
[ drop ] keep      =
dup [ drop ] dip   =
rot unrot          =
unrot rot          =
[ swap swap ] dip  =
[ swap swap ] call =
```

`swap swap` and `[ swap swap ] call` take two values. `rot unrot`, `unrot rot`, and `[ swap swap ] dip` take three. `[ swap swap ] dip` hides the top value while the pair under it is restored. The others take the top value.

`5 quote call` is `5`. `next dup drop`, `next quote call`, and `[ next ] call` are the one call `next`.

## One permutation

```text
nip         =  swap drop
nip         =  [ drop ] dip
over        =  dupd swap
tuck        =  swap over
drop drop   =  drop2
```

`nip` leaves the top of two values. `over` copies the value under the top. `tuck` copies the top value under the one below it. `drop drop` leaves nothing.

## A quotation that stays a value

`[ swap swap ]`, returned and not called, is the function of two arguments that returns them in order.

`[ swap swap ] cons cons` captures two values. The quotation it leaves takes nothing and returns those values in the order they were captured.

```ply
: exchange ( a b. a b -- b a )
  swap

: restored ( Int Int -- Int Int )
  exchange exchange
```

`exchange` itself returns its arguments reversed. `restored` calls `exchange` twice.
