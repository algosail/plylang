# Words

These words are in every module. They do not need an import. Operations on values, such as arithmetic, are imported and are not in this list.

The top of the stack is the rightmost name. On `x y`, `y` is the top. `..s` is the stack under the named values, and a word that writes `..s` on both sides puts that part back unchanged. `..t` is the stack a quotation left. A quotation is a value of a stack type, written `(in -- out)`.

## Stack

### dup

```ply
..s a -- ..s a a
```

Copies the top value. On `x y` the result is `x y y`.

### drop

```ply
..s a -- ..s
```

Drops the top value. On `x y` the result is `x`.

### swap

```ply
..s a b -- ..s b a
```

Swaps the top two values. On `x y` the result is `y x`.

### nip

```ply
..s a b -- ..s b
```

Drops the value under the top. On `x y` the result is `y`.

### over

```ply
..s a b -- ..s a b a
```

Copies the value under the top onto the top. On `x y` the result is `x y x`.

### dupd

```ply
..s a b -- ..s a a b
```

Copies the value under the top. On `x y` the result is `x x y`.

### swapd

```ply
..s a b c -- ..s b a c
```

Swaps the two values under the top. On `x y z` the result is `y x z`.

### tuck

```ply
..s a b -- ..s b a b
```

Copies the top value under the one below it. On `x y` the result is `y x y`.

### pick

```ply
..s a b c -- ..s a b c a
```

Copies the third value onto the top. On `x y z` the result is `x y z x`.

### rot

```ply
..s a b c -- ..s b c a
```

Moves the third value to the top. On `x y z` the result is `y z x`.

### unrot

```ply
..s a b c -- ..s c a b
```

Moves the top value under the next two. On `x y z` the result is `z x y`.

### dup2

```ply
..s a b -- ..s a b a b
```

Copies the top two values. On `x y` the result is `x y x y`.

### drop2

```ply
..s a b -- ..s
```

Drops the top two values.

### nip2

```ply
..s a b c -- ..s c
```

Drops the two values under the top. On `x y z` the result is `z`.

### swap2

```ply
..s a b c d -- ..s c d a b
```

Swaps the top two pairs. On `a b c d` the result is `c d a b`.

### over2

```ply
..s a b c d -- ..s a b c d a b
```

Copies the pair under the top pair onto the top. On `a b c d` the result is `a b c d a b`.

## Quotations

### dip

```ply
..s a (..s -- ..t) -- ..t a
```

Hides the value under the quotation, runs the quotation, and restores that value on top. The quotation is already on top.

### dip2

```ply
..s a b (..s -- ..t) -- ..t a b
```

Hides two values under the quotation and restores them in the same order.

### dip3

```ply
..s a b c (..s -- ..t) -- ..t a b c
```

Hides three values under the quotation and restores them in the same order.

### keep

```ply
..s a (..s a -- ..t) -- ..t a
```

Runs the quotation, then pushes back the value it consumed. `5 [ drop 1 ] keep` leaves `1`, then `5`.

### keep2

```ply
..s a b (..s a b -- ..t) -- ..t a b
```

Runs the quotation, then pushes back the two values it consumed.

### bi

```ply
..s a (a -- b) (a -- c) -- ..s b c
```

Applies two quotations to one value. Each quotation returns one value. `5 [ drop 1 ] [ drop 2 ] bi` leaves `1`, then `2`.

### tri

```ply
..s a (a -- b) (a -- c) (a -- d) -- ..s b c d
```

Applies three quotations to one value. Each quotation returns one value.

### bis

```ply
..s a b (a -- c) (b -- d) -- ..s c d
```

The deeper quotation takes the deeper value, and the top quotation takes the top value. Each returns one value.

### bia

```ply
..s a a (a -- b) -- ..s b b
```

Applies one quotation to each of two values of the same type.

### call

```ply
..s (..s -- ..t) -- ..t
```

Pops the quotation and runs it on the rest of the stack. Each named input of that quotation must already be a solved type.

### quote

```ply
..s a -- ..s (-- a)
```

Wraps the top value in a quotation that pushes it again. `5 quote` has type `(-- Num)`.

### if

```ply
..s Bool (..s -- ..t) (..s -- ..t) -- ..t
```

Pops a `Bool` and two quotations. The `true` branch is pushed first, so the `false` branch is on top. The two branches leave the same stack.

## Value

### Unit

```ply
..s -- ..s Unit
```

Pushes the only value of type [`Unit`](Types.md#unit).
