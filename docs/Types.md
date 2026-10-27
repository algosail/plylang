# Types

Seven types are installed in every module. A data declaration or a synonym cannot reuse one of these names.

A name with no parameter is written bare. `Num` and `(Num)` are the same type. A parameter is written in parentheses: `(Array Num)`.

A record is an unnamed row of fields. A quotation is a stack effect between `[` and `]`. Those forms are not names in this list.

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

No parameters. The only value is the word [`Unit`](Words.md#unit). JavaScript emits `undefined`.

That word is also the constructor, so `Unit ->` matches the value.

## Array

One parameter, written `(Array a)`. A literal lists the elements between `<` and `>`. The left element is index 0. `<>` is empty.

```ply
: nums ( -- (Array Num) )
  < 1 2 3 >
```

An element is a literal, a quotation, a nested array, or a record.

JavaScript emits an array.

A pattern has one fixed length. `<>` matches only the empty array. One length does not cover every `Array`. A later `_` covers the other lengths.

## StrMap

One parameter, written `(StrMap a)`. The key is `Str`. The program chooses the key when it runs, and the type does not name that key. A value of this type comes from an imported word.

## Foreign data

`; foreign` declares a type with no constructors. JavaScript emits nothing for it. The values come from `: foreign` words.

A parameter indexes the value. `(Hub Num)` and `(Hub Str)` are different types. The parameter is not a field.

```ply
; foreign Hub a
```

A name binds the value. There is no constructor to match. Writing `=`, `==`, or a constructor is [PL0116](Errors.md#pl0116).
