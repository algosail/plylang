# Types

Seven types are installed in every module. A data declaration or a synonym cannot reuse one of these names.

A name with no parameter is written bare. `Num` and `(Num)` are the same type. A parameter is written in parentheses: `(Array Num)`.

A function type is a stack effect, `( -- a )`. It is not a name in this list.

## Int

No parameters. A literal is a plain integer and the suffix `n`: `0n`, `42n`, `-42n`. `1name` is the number `1` and the name `name`. A fraction or an exponent is [`Num`](#num), so the suffix `n` is not part of it.

JavaScript emits a `bigint`.

An equation does not cover every `Int`. The last arm is a name or `_`.

## Num

No parameters. A literal is a JavaScript number: `1`, `42`, `1.5`, `1e-3`, `-2.5`. `1.0` is a number. `1.` is the number `1` and a separate `.`.

JavaScript emits a number.

An equation does not cover every `Num`. The last arm is a name or `_`.

## Str

No parameters. A literal is a double-quoted string: `"hi"`, `"a\nb"`. The escapes are `\\`, `\"`, `\n`, `\t`, and `\r`.

JavaScript emits a string.

An equation does not cover every `Str`. The last arm is a name or `_`.

## Bool

No parameters. The values are `true` and `false`. Those two arms cover `Bool`.

JavaScript emits a boolean.

## Unit

No parameters. The only value is the literal `unit`. JavaScript emits `undefined`. `null` is not `Unit`.

`unit ->` checks the value and consumes it. One such arm covers `Unit`. `Unit ->` leaves the value and calls the `Nullary` word. On a wider type it does not cover the slot.

## Array

One parameter, written `(Array a)`. A literal is a program between `<` and `>`. It starts from an empty stack and does not see the stack outside. The values it leaves are the elements. The left element, index 0, is the bottom value. `<>` is empty. Every element has one type.

```ply
: nums ( -- (Array Num) )
  < 1 2 3 >

: paired ( -- (Array Num) )
  < 1 dup >
```

`< 1 2 add >` is one number, once `add` is in scope. A quotation is a value. `< [ dup drop ] [ dup drop ] >` is an array of quotations, and the quotations do not run until something calls them. A pattern does not run the program.

JavaScript emits an array. `dup` shares that reference. A word that writes returns a new array, and the array it received stays as it was. A shared cell that a program passes is `Cell`, from the prelude.

A pattern has one fixed length. `<>` matches only the empty array. One length does not cover every `Array`. A later `_` covers the other lengths. `Array ->` covers every array when `Unary` for `Array` is visible. It calls that instance's word and leaves the array.

## Matching a shape

A bare name that is not a constructor of the slot matches a visible `Nullary`, `Unary`, or `Binary` instance and leaves the value. The class is the arity of the head. The prelude gives `Nullary` to `Num`, `Int`, `Str`, `Bool`, and `Unit`, and `Unary` to `Array`. A `; foreign` type is matched the same way when its module writes the instance. Until that module is imported, the pattern is `no instance for Nullary Buffer`, or `no instance for Unary Hub`. `Cell` has no instance.

`(Array Num)` tests the array and then every element, through that instance's parameter word. Nested `(Array (Array Num))` follows. `(Array x)` and `(Array _)` do not name the element. `(Map Str Num)` needs `Binary`, and `(Map _ Num)` tests only the second parameter. `(Hub Num)` is `no instance for Unary Hub` until that module defines `Unary`. Write the head until then.

The arm is checked against the word's signature with those variables filled in. The other arms and the caller keep the original signature. The compiler does not prove two checks are disjoint, so on a variable slot both `Num ->` and `Str ->` stay, and a later `_` is still required.

```ply
: held ( a. a -- a )
  Num ->
  _ ->
```

`Num ->` is `( Num -- Num )`. `_ ->` is `( a -- a )`. Both leave the value. `1 held` is `Num`. `"hi" held` is `Str`.

`Array ->` in `( Foldable f => (f a) -- Num )` is `( (Array a) -- Num )`. `f` is `Array`. The element stays `a`. A shape does not cover a wider slot, so a later `_` is still required. `_` leaves the value.

## Foreign data

`; foreign` declares a type with no constructors. JavaScript emits nothing for it. The values come from `: foreign` words. Match the type when its module gives it `Nullary`, `Unary`, or `Binary`. The pattern calls that module's word.

A parameter indexes the value. `(Hub Num)` and `(Hub Str)` are different types. The parameter is not a field, and it is not an element test.

```ply
; foreign Hub a
```

A name binds the value. There is no constructor to match. Writing `=`, `==`, or a constructor is [PL0116](Errors.md#pl0116).
