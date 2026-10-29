# Ply in 5 minutes

Ply is a concatenative language. A program is a sequence of words. A word takes values off the single stack and puts new ones on. The type of a word is that transformation. In stack notation the top is always on the right.

The compiler translates a module into JavaScript. There is no separate virtual machine. The language version is 1. There are no subtypes: two types are compatible when they unify.

This text walks through the language once, with a short example of each feature.

The repository contains the `plylang` compiler and the `plylang-ls` server. Words that work on ready-made values are not built into the compiler: they are imported. The neighboring package `@algosail/ply-prelude` is one such place, and it has its own documentation. The imports in the examples below illustrate the notation.

## Stack

A signature says what a word takes and what it puts. The tail under that prefix stays in place.

```ply
~ "Num" (add)

: double ( Num -- Num )
  dup add
```

The body runs from left to right. A number is on the stack. `dup` copies the top. `add` takes both numbers and puts the sum.

A word starts with `:`. Then `foreign` and `private` may appear, in either order and each at most once, followed by the name and the scheme in parentheses. Spaces inside the parentheses do not matter. The body may continue on the same line: `: double ( Num -- Num ) dup add`.

A name in a scheme is one slot. `Num Num` is two slots. `Maybe a` is also two: the constructor needs an argument, and that argument is written in parentheses, `(Maybe a)`. `(Num)` is the same type as `Num`.

A lowercase name is a scheme variable, in order of appearance. An explicit list before the dot is also accepted: `a b. a b -- b`. The kind is inferred. A stack slot has kind `Type`. In `(f a)` the kind of `f` is `Type -> Type`.

An omitted stack bottom is the same tail on the left and on the right. An explicit empty bottom is written with a dot: `. a -- . a` requires a stack of exactly one value. The tail `..s` takes the rest as a whole. `..s` and `..s a` are different schemes.

## Values

Five built-in types and two constructors of arity 1 are visible in every module. They cannot be redefined.

| Type | Literal | In JavaScript |
| --- | --- | --- |
| `Num` | `1`, `42`, `1.5`, `1e-3` | number |
| `Int` | `0n`, `42n`, `-42n` | `bigint` |
| `Str` | `"hi"`, `"a\nb"` | string |
| `Bool` | `true`, `false` | boolean |
| `Unit` | the word `Unit` | `undefined` |

`(Array a)` is an array. `(StrMap a)` is a dictionary with a string key. Bare digits are `Num`, an ordinary number in JavaScript. `Int` is written with the suffix `n` and becomes a `bigint`. In the examples below a number is `Num`. `1.0` is a number. `1.` is the number `1` and a separate dot. A string accepts `\\`, `\"`, `\n`, `\t`, and `\r`.

`Unit` is both a type and a word. The word puts the single value of that type.

Operations on these values are imported. This text does not list them.

## Stack words

These words are built in. They need no import. On a stack `x y` the top is `y`:

```ply
: copy ( a -- a a )
  dup

: gone ( a b -- a )
  drop

: flipped ( a b -- b a )
  swap

: top ( a b -- b )
  nip

: again ( a b -- a b a )
  over
```

The same permutations as in Factor follow, with names made of letters: `dupd`, `swapd`, `tuck`, `pick`, `rot`, `unrot`, `dup2`, `drop2`, `nip2`, `swap2`, `over2`. A name does not start with a digit, so the pair is written `dup2`. On `x y z` with `z` on top, `rot` yields `y z x` and `unrot` yields `z x y`.

## Quotation

`[ ... ]` is a value. It does not capture the stack outside, and it does not run by itself. Square brackets are a quotation. An array is written in angle brackets.

```text
: call ( ..s (..s -- ..t) -- ..t )
: dip ( ..s a (..s -- ..t) -- ..t a )
: keep ( ..s a (..s a -- ..t) -- ..t a )
: bi ( ..s a (a -- b) (a -- c) -- ..s b c )
: if ( ..s Bool (..s -- ..t) (..s -- ..t) -- ..t )
: quote ( ..s a -- ..s (-- a) )
: cons ( ..r a (..s a -- ..t) -- ..r (..s -- ..t) )
```

`call` takes a quotation and runs it on the rest. `dip` hides the value under the quotation, runs the quotation, and returns what it hid to the top. `dip2` and `dip3` hide two and three values. `keep` runs the quotation and returns the value it took. `keep2` returns two.

`bi` applies two quotations to one value. Each returns exactly one. `tri` does the same with three quotations. `bis` gives the deeper value to the first quotation and the top to the second. `bia` applies one quotation to each of two values of the same type.

`if` takes a `Bool` and two quotations. The `true` branch is placed first and the `false` branch second, so `false` ends up on top. The branches agree on the stack.

`quote` wraps the top. `42 quote` is a quotation of type `(-- Num)`.

`cons` pushes a value and then runs a quotation. The value sits under the quotation, so `5 [ 1 add ] cons` is a quotation of type `(-- Num)` that pushes five and then adds one. The call site does not name that value.

```ply
~ "Num" (eq, sub, mul)

: fact ( Num -- Num )
  dup 0 eq [ drop 1 ] [ dup 1 sub fact mul ] if
```

Zero is replaced by one. Otherwise `n` and `n - 1` remain on the stack, the recursive `fact` computes the second, and `mul` multiplies.

Names before `->` name stack values. The left name is the deeper value, the right name is the top. A quotation sees these names, and they are not part of the input.

```ply
: curry ( a (a b -- c) -- (b -- c) )
  x q ->
    [ x swap q call ]
```

This is an ordinary word of a module. It is not built into the compiler.

`call` requires a known finite length for the quotation's input.

## Pattern matching

When the first line of a body is patterns and `->`, the whole body is equations. The right-hand pattern is the top. A name binds a slot. A literal is compared with the value. `_` discards a value. The first match wins. A value cannot be matched in the middle of a body: write a separate word for that, and make its whole body equations.

```ply
: toString ( Bool -- Str )
  true -> "true"
  false -> "false"
```

`true` together with `false` covers `Bool`. `Num`, `Int`, and `Str` need a final branch with a name or `_`.

A bare constructor puts its fields on the stack from left to right, the last field on top. Parentheses hold one slot: `(Just x)` names the field and does not put it until the name is mentioned. `(Person n _)` names the first field of `Person` and discards the second.

An array in a pattern has a fixed length. `<>` matches only the empty array. One length does not cover every `Array`.

```ply
: isEmpty ( (Array a) -- Bool )
  <> -> true
  _ -> false
```

## Data

`;` in column 0 names an algebraic type. A constructor is a word. A type and a word with one name live in different spaces.

```ply
; Maybe a = Nothing | Just a

: fromMaybe ( a (Maybe a) -- a )
  Nothing ->
  Just -> nip

: mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just
```

The empty body of the `Nothing` branch leaves the fallback value. For `Just` the field lands above it, and `nip` drops what is underneath. In `mapMaybe` the quotation is the top: `q` names it, and `(Just x)` names the field.

A list with its own structure is a separate type. The head sits in the constructor, and the tail is another value of that type.

```ply
; List a = Nil | Cons head : a, tail : (List a)
```

Zero fields and one field have no names. `Nothing` stores nothing. `Just a` stores one value, and the equation names it. Two or more fields are `name : Type`, separated by commas. A second positional type is [PL0117](docs/Errors.md#pl0117). One named field is [PL0118](docs/Errors.md#pl0118).

```ply
; Pair a b = Pair frs : a, snd : b

: frs ( a b. (Pair a b) -- a )
  frs>>

: setSnd ( a b. (Pair a b) b -- (Pair a b) )
  >>snd
```

`Pair` the word takes the fields in that order, last on top. `frs>>` consumes the pair and leaves the field. When the pair is still needed, copy it first: `dup frs>>`. `>>snd` takes the new field above the pair and returns a new pair. The previous pair is a separate value. A sum has no reader: `->` picks the constructor. `name>>` on `Maybe` is [PL0119](docs/Errors.md#pl0119). `frs>>` on `Int`, on a type variable, or on a synonym of `Num` is [PL0063](docs/Errors.md#pl0063). A missing field is [PL0120](docs/Errors.md#pl0120).

In JavaScript a constructor with no fields is `{ tag: "Nothing" }`. One field is `(value) => ({ tag: "Just", value })`. Named fields are properties: `(v0, v1) => ({ tag: "Pair", frs: v0, snd: v1 })`.

## Array

`< 1 2 3 >` is an array of three numbers, of type `(Array Num)`. Index 0 is the left element. The empty array is `<>`. A word is not written inside the angle brackets: an element is a literal, a quotation, or a nested array.

A word that walks an array is imported. In the example the quotation sits on top. The name comes from one module, and the instance for the array comes from another.

```ply
~ "Map" (map)
~ "Num" (add)
~ "Arr" ()

: shifted ( -- (Array Num) )
  < 1 2 3 > [ 1 add ] map
```

Empty parentheses bring the instances of a module and bring no names. The result is `2`, `3`, `4`.

## A name for a type

`-` names a type that already exists. The right-hand side follows immediately. It is the same type, and the name has no constructor.

```ply
; Person = Person name : Str, age : Num
- Human Person

: older ( Human -- Human )
  37 >>age
```

Parameters are written as they are on a type: `- Twin a (Pair a a)`. In a slot a synonym is saturated, `(Twin Num)`.

## Wrapper

`;` with `==` is a distinct type with one constructor and one field. It does not unify with the field. In JavaScript the constructor returns its argument as it is.

```ply
~ "Num" (add)

; Age == Age Num

: older ( Age -- Age )
  Age -> 1 add Age
```

Zero fields, two fields, or two constructors do not fit `==`. A type with `=` is for those.

## Dictionary

`(StrMap a)` is a dictionary with key `Str`. The key becomes known at run time and is not named in the type ahead of time. There is no literal. An empty dictionary in JavaScript is `Object.create(null)`.

```ply
~ "Dict" (empty, put)

: ages ( -- (StrMap Num) )
  empty "Ada" 36 put
```

The order in the example is the dictionary, the key, and the value on top.

## Classes

`&` declares a class. A method is written without a colon. A constraint is not written in the class text: the compiler adds it. `#` declares an instance. The head is the type after the class name. Superclasses stand to the left of `=>`. A subclass constraint brings a superclass method when that method's name is imported. Several constraints in a scheme are separated by a comma.

```ply
& Equal a
  eq ( a a -- Bool )

& Equal a => Ordered a
  before ( a a -- Bool )
```

```ply
~ "Equal" (Equal, eq)
~ "Num" ()

: same ( Equal a => a a -- Bool )
  eq
```

The instance for numbers is declared in the `Num` module. `()` brings the instances of that module and brings no names. The empty import of `Num` gives `same` that instance.

A hand-written instance is a word for each method. A module may write one when it defines the class, the type, or the wrapper in the head, or when the head contains a built-in constructor `Int`, `Num`, `Bool`, `Str`, `Unit`, `Array`, or `StrMap`.

```ply
& Equal a
  eq ( a a -- Bool )

; Box a = Mk a

: eqBox ( Equal a => (Box a) (Box a) -- Bool )
  (Mk x) (Mk y) -> x y eq

# Equal (Box a)
  eq = eqBox
```

A parameter of kind `Type -> Type` writes the constructor in the head.

```ply
& Mapped f
  map ( (f a) (a -- b) -- (f b) )
```

An instance of such a class begins with `# Mapped Box`.

Two classes with one head are written with a comma. An instance arrives only through a direct import: it does not pass through a second module.

When an exported word has a constraint in its scheme, the method dictionary becomes the first argument in JavaScript.

## Module

One `*.ply` file is one module. The file itself has no name. The path from the source directory gives it: `src/Data/Maybe.ply` is called `Data.Maybe`. Each segment starts with a capital Latin letter.

Imports stand at the beginning of the file. A path inside this package is written from the source directory. A path into another package is a package specifier. `..` in the parentheses means every public name.

```ply
~ "Data/Maybe" (Just, Nothing, fromMaybe)
~ "@algosail/example/Num" (add)
```

`private` is visible in this file, is written into `.d.ply`, and is not exported.

A comment runs from `.` to the end of the line, when the dot is not inside `()`. `..` in column 0, directly above `:`, `;`, `-`, or `&`, is the text of that declaration. One space after `..` is not part of the text. A type has one text for all of its constructors. A class has one text for its name and for all of its methods. A `..` line above `#` does not attach to the declaration.

A declaration stands in column 0 and starts with `:`, `;`, `-`, `&`, `#`, or `~`.

A package is described by `package.json`.

```json
{
  "name": "@algosail/example",
  "type": "module",
  "ply": { "source": "src", "foreign": "foreign", "out": "dist", "language": 1 },
  "exports": {
    "./Data/Maybe": {
      "ply": "./dist/Data/Maybe.d.ply",
      "import": "./dist/Data/Maybe.js"
    }
  }
}
```

A `foreign` word has no body. Its implementation lives in the `ply.foreign` directory, and the number of JavaScript parameters matches the input length. It combines with `private`.

```ply
: foreign private length ( (Array a) -- Num )
```

`ply.foreign` is required when the package has such a word. The directory lies inside the package and is distinct from `source` and `out`. The generated `.js` imports the implementation from there. Those files are not copied into `dist`.

```text
plylang check [dir]
plylang build [dir] [--exports]
plylang test [pattern]
plylang docs [dir] [--out <file>]
plylang check --parse <file>
plylang check --js <file>
```

`plylang check` checks a package the same way `build` does: every file under `ply.source`, the imports, and `foreign`. It writes no files. The default directory is the current one. After the same check, `plylang build` writes a `.js` file and a `.d.ply` interface into `ply.out`. `plylang build --exports` replaces the `exports` field of `package.json` with entries for the built modules. A key is `./` followed by the module path. `ply` points at the `.d.ply`, and `import` points at the `.js`. The other fields of the file stay as they were. A module of this package is imported by its path under `ply.source`, whether or not `exports` lists it. Another package finds the module through the `exports` entry.

`plylang test` builds the package, then compiles and runs the files that match one pattern. The pattern is relative to the package root. The default is `test/**/*.ply`. `*` matches inside one path segment. `**` matches across directories. Quote the pattern so the shell does not expand `*`. A test is a public word `( -- Verdict )`. The runner calls each one in source order and uses the word as its name. A `private` word is not a test. `expect` compares two values. `pass` and `differ` build a verdict when those values have no `Showable` instance. The words live in `@plylang/test`. A file with no such word may export `suite` with scheme `( -- (Array Verdict) )` instead. A file under `ply.source` stays where `build` wrote it. Any other test file is compiled in that package, written under `ply.out` for the run, and deleted afterwards. The report names each file and each check. A check also prints `file:line` of its word. A failing check prints its text under the name. One check that throws still lets the other checks run. A file that does not compile or does not run prints the diagnostic, and the other files still run. A failed check, a compile error, or a thrown suite exits 1. No file matched exits 1 with `no tests matched`. Color is used when stdout is a terminal and `NO_COLOR` is unset.

A word named `main` whose scheme is `( -- Unit )` is the program of that module. It may be `private`. `build` calls it at the bottom of that module's `.js`, and only when Node started that file. `node dist/App.js` runs the program. An import of the module does not. A `package.json` script can name that file. Several modules may each have a `main`. A module without one is a library. Any other scheme on the name `main` is an error.

`plylang docs` writes `DOCS.md` at the package root. `--out` names another file. A module heading is the string used to import it: the package name and the path, or only the path when there is no name. The prose comes from `..`. Inside a module the sections are `Types`, `Classes`, `Instances`, and `Words`. An empty section is left out. The name of a type, a class, or a word is a heading under its section. `Instances` is a list of `#` heads, with no comment and no method bindings.

`plylang check --parse` prints how one file was parsed, and it does not check types. `plylang check --js` prints the JavaScript that `build` would write for that file. Outside a package that is the JavaScript of the one file, and imports are not resolved.

The exit code is 0 for success, 1 for a diagnostic, and 2 for bad arguments. `--help` prints the same lines. Messages are in English: `file:line:column: error: PLxxxx: message`, and the next line is a link to the matching section of `docs/Errors.md`.

What enters a word's function is its own prefix, from the bottom upward. One result is returned by itself. Several results come back as a JavaScript array: that is the calling convention, and it is not an `(Array a)` value. The words from `dup` through `over2` do not become calls. `call`, `dip`, `keep`, `bi`, `if`, and their neighbors inline the body of the quotation. `cons` builds a quotation that pushes the value under it and then runs it.

## Building

From this repository:

```text
cargo build --release
```

`target/release/plylang` and `target/release/plylang-ls` are the compiler and the server. The server speaks LSP over stdio. In a package with a `ply` field it resolves imports the same way `plylang build` does.

The npm package is named `@plylang/plylang` and installs the same two commands. Version `1.x` of that package is language `1`. A minor or a patch of the compiler does not change the language number. `PLYLANG_BIN` and `PLYLANG_LS_BIN` supply a binary of their own.

Highlighting is the [tree-sitter-ply](https://github.com/algosail/tree-sitter-ply) grammar. The Zed extension is the neighboring repository `zed-ply`: install it as a dev extension after `cargo build -p plylang-ls`.

The license is MIT.
