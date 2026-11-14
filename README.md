# Ply in 5 minutes

Ply is a concatenative language. A program is a sequence of words. A word takes values off the single stack and puts new ones on. The type of a word is that transformation. In stack notation the top is always on the right.

The compiler translates a module into JavaScript. There is no separate virtual machine. The language version is 1. There are no subtypes: two types are compatible when they unify.

This text walks through the language once, with a short example of each feature.

The repository contains the `plylang` compiler and the `plylang-ls` server. They are separate npm packages. `@plylang/plylang` is the compiler. `@plylang/language-server` is the server. The server command remains `plylang-ls`. A library depends on the compiler. The server is installed by the editor. Words that work on ready-made values are not built into the compiler: they are imported. The neighboring package `@plylang/prelude` is one such place, and it has its own documentation. The import lines below name that package. They check in a package that depends on it.

## Stack

A signature says what a word takes and what it puts. The tail under that prefix stays in place.

```ply
~ "@plylang/prelude/Classes/Arithmetic" add
~ "@plylang/prelude/Data/Num"

: double ( Num -- Num )
  dup add
```

The body runs from left to right. A number is on the stack. `dup` copies the top. `add` takes both numbers and puts the sum.

A word starts with `:`. Then `foreign` may appear, followed by the name and the scheme in parentheses. Spaces inside the parentheses do not matter. The body may continue on the same line: `: double ( Num -- Num ) dup add`.

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
~ "@plylang/prelude/Classes/Comparable" equal
~ "@plylang/prelude/Classes/Subtractable" subtract
~ "@plylang/prelude/Classes/Arithmetic" multiply
~ "@plylang/prelude/Data/Num"

: fact ( Num -- Num )
  dup 0 equal [ drop 1 ] [ dup 1 subtract fact multiply ] if
```

Zero is replaced by one. Otherwise `n` and `n - 1` remain on the stack, the recursive `fact` computes the second, and `multiply` multiplies.

Names before `->` name stack values. The left name is the deeper value, the right name is the top. A quotation sees these names, and they are not part of the input. A clause that only renames the stack, and whose names do not appear in a quotation, is ordinary stack code written without `->`.

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

A bare constructor checks the tag and leaves the value. `(Ctor ..)` spills every field, last on top. `(Just x)` names the one field and does not put it until the name is mentioned. `(Person n _)` is positional: the first field is kept under `n` and the second is discarded. `(Person age name)` binds by name, in either order, when every name is a field. `(Person)` is an error. `(Some (Array Num) items)` checks that the field is an array of numbers and names that array. The check runs when the field's type is still open. `(Some (Array Num))` without a name is an error. `(Some (Num) ..)` checks and spills that number. `(Some (Num) _)` only checks.

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
  (Nothing ..) ->
  (Just ..) -> nip

: mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )
  (Nothing ..) q -> q drop Nothing
  (Just x) q -> x q call Just
```

The empty body of the `(Nothing ..)` branch leaves the fallback value. For `Just`, `(Just ..)` puts the field above it, and `nip` drops what is underneath. In `mapMaybe` the quotation is the top: `q` names it, and `(Just x)` names the field. A bare `Just` would leave the `Maybe` in place.

A list with its own structure is a separate type. The head sits in the constructor, and the tail is another value of that type.

```ply
; List a = Nil | Cons head : a, tail : (List a)
```

A `|` may stand before the first constructor, so every arm starts in the same column.

Zero fields and one field have no names. `Nothing` stores nothing. `Just a` stores one value, and the equation names it. Two or more fields are `name : Type`, separated by commas. A second positional type is [PL0117](docs/Errors.md#pl0117). One named field is [PL0118](docs/Errors.md#pl0118).

```ply
; Pair a b = Pair frs : a, snd : b

: frs ( a b. (Pair a b) -- a )
  frs>>

: setSnd ( a b. (Pair a b) b -- (Pair a b) )
  >>snd
```

`Pair` the word takes the fields in that order, last on top. `frs>>` consumes the pair and leaves the field. When the pair is still needed, copy it first: `dup frs>>`. `>>snd` takes the new field above the pair and returns a new pair. The previous pair is a separate value. A field on every constructor, with the same type, is `name>>`. A field of one constructor is read after that constructor's check: `Sent -> body>>`. `name>>` on `Maybe` is [PL0119](docs/Errors.md#pl0119). `frs>>` on `Int`, on a type variable, or on a synonym of `Num` is [PL0063](docs/Errors.md#pl0063). A missing field is [PL0120](docs/Errors.md#pl0120). `(Page)` is [PL0136](docs/Errors.md#pl0136).

In JavaScript a constructor with no fields is `{ tag: "Nothing" }`. One field is `(value) => ({ tag: "Just", value })`. Named fields are properties: `(v0, v1) => ({ tag: "Pair", frs: v0, snd: v1 })`.

## Array

`< 1 2 3 >` is an array of three numbers, of type `(Array Num)`. Index 0 is the left element. The empty array is `<>`.

The text between the angles is a program. It starts from an empty stack and does not see the stack outside. The values it leaves are the elements, and they must be one type. `< 1 dup >` is two numbers. `< 1 2 add >` is one number, once `add` is in scope. A quotation is a value, so `< [ 1 add ] [ 2 add ] >` is an array of quotations. `[ call ] map` runs those. A pattern does not run this program. `<>` matches only the empty array.

A word that walks an array is imported. In the example the quotation sits on top. The name comes from one module, and the instance for the array comes from another.

```ply
~ "@plylang/prelude/Classes/Mappable" map
~ "@plylang/prelude/Classes/Arithmetic" add
~ "@plylang/prelude/Data/Num"
~ "@plylang/prelude/Data/Array"

: shifted ( -- (Array Num) )
  < 1 2 3 > [ 1 add ] map
```

The bare import of `Data/Array` brings the instances of that module and brings no names. The result is `2`, `3`, `4`. `dup` of an array shares the JavaScript reference. A word that writes returns a new array, and the input stays.

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
~ "@plylang/prelude/Classes/Arithmetic" add
~ "@plylang/prelude/Data/Num"

; Age == Age Num

: older ( Age -- Age )
  (Age ..) -> 1 add Age
```

Zero fields, two fields, or two constructors do not fit `==`. A type with `=` is for those.

## Dictionary

`(StrMap a)` is a dictionary with key `Str`. The key becomes known at run time and is not named in the type ahead of time. There is no literal. An empty dictionary in JavaScript is `Object.create(null)`.

```ply
~ "@plylang/prelude/Data/StrMap" emptyStrMap, insertStrMap

: ages ( -- (StrMap Num) )
  emptyStrMap "Ada" 36 insertStrMap
```

The order in the example is the dictionary, the key, and the value on top.

## Classes

`&` declares a class. A method is written without a colon. A constraint is not written in the class text: the compiler adds it. `#` declares an instance. The head is the type after the class name. Superclasses stand to the left of `=>`. A subclass constraint brings a superclass method when that method's name is imported. Several constraints in a scheme are separated by a comma.

```ply
& Equal a
  equal ( a a -- Bool )

& Equal a => Ordered a
  before ( a a -- Bool )
```

```ply
~ "@plylang/prelude/Classes/Comparable" Comparable, equal
~ "@plylang/prelude/Data/Num"

: same ( Comparable a => a a -- Bool )
  equal
```

The instance for numbers is declared in `@plylang/prelude/Data/Num`. The bare import brings the instances of that module and brings no names. That import gives `same` that instance.

A hand-written instance is a word for each method. A module may write one when it defines the class, the type, or the wrapper in the head, or when the head contains a built-in constructor `Int`, `Num`, `Bool`, `Str`, `Unit`, `Array`, or `StrMap`.

```ply
& Equal a
  equal ( a a -- Bool )

; Box a = Mk a

: eqBox ( Equal a => (Box a) (Box a) -- Bool )
  (Mk x) (Mk y) -> x y equal

# Equal (Box a)
  equal = eqBox
```

A parameter of kind `Type -> Type` writes the constructor in the head.

```ply
& Mapped f
  map ( (f a) (a -- b) -- (f b) )
```

An instance of such a class begins with `# Mapped Box`.

Two classes with one head are written with a comma. An instance is not a name. Any import carries the instances of that module, and those instances pass along the next import. The word on the right of `=` stays in the file that writes `#` unless that name is also in the `@` list. The caller writes the class method.

When an exported word has a constraint in its scheme, the method dictionary becomes the first argument in JavaScript.

## Module

One `*.ply` file is one module. The file itself has no name. The path from the source directory gives it: `src/Data/Maybe.ply` is called `Data.Maybe`. Each segment starts with a capital Latin letter.

A module names what it publishes with one `@` list. The list may stand anywhere in the file. A name in the list leaves the file. Any other name stays in the file. The same list holds words, types, constructors, synonyms, classes, methods, and names brought in by an import. A type and its constructor share one entry when they share a name. Names are separated by commas. A bare `@` publishes nothing. A second `@` is a duplicate definition.

```ply
~ "Data/Maybe" Just, Nothing, fromMaybe
~ "@algosail/example/Num" add

@ Maybe, Nothing, Just, fromMaybe
```

Imports stay together and come before the first `:`, `;`, `-`, `&`, `#`, `?`, or `=`. A path inside this package is written from the source directory. A path into another package is a package specifier. The names after the path are what this file uses. `: Alias` brings every published name as `Alias:name`, and a call is `Alias:send`. `send : busSend` brings `send` as `busSend`. A bare path brings instances and no names. Writing the local name again in the `@` list publishes it. A colon in column 0 still starts a word.

A word `main` with scheme `( -- Unit )` may stay off the list. Node still runs it when it starts that file.

A comment runs from `.` to the end of the line, when the dot is not inside `()`. `..` in column 0, directly above `:`, `;`, `-`, `&`, `?`, or `=`, is the text of that declaration. One space after `..` is not part of the text. A line that is only `..` keeps a paragraph break. A type has one text for all of its constructors. The lines above `&` are the text of the class. A `..` line at the start of an indented line, directly above a method, is the text of that method. A method without that line keeps the class text in the hover. A `..` line above `#` does not attach to the declaration.

A declaration stands in column 0 and starts with `@`, `:`, `;`, `-`, `&`, `#`, `?`, `~`, or `=`.

A place is one value the module keeps off the stack. `= base Num` declares it. The program under that line has scheme `( -- Num )` and runs once, when the module loads. After that, `base` reads the current frame `( -- Num )`. `=base` writes the top value `( Num -- )`. `=>base` runs the quotation under a new frame and restores the previous frame afterwards, including when the quotation throws: `( ..s Num ( ..s -- ..t ) -- ..t )`. The stored type is monomorphic. Importing `base` also opens `=base` and `=>base`. The program that fills the place does not read or write it.

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

A `foreign` word has no body. Its implementation lives in the foreign directory, and the number of JavaScript parameters matches the input length. Leave it out of the `@` list when it stays in this file. A foreign word that receives an array or a `StrMap` does not change that value. It copies, then writes. `dup` shares the reference. `Cell` is the type of a shared cell.

```ply
: foreign length ( (Array a) -- Num )
```

The foreign directory is `foreign` when that directory exists, or the string `ply.foreign` when the field is set. It is required when the package has such a word. It lies inside the package and is distinct from the source directory and the output directory. The generated `.js` imports the implementation from there. Those files are not copied into `dist`. A new package writes `ply.language` and leaves `source`, `foreign`, and `out` unset: those directories are `src`, `foreign`, and `dist`. `ply.entry` names the module a pathless package import opens, and the `exports` key `.`. `plylang compile` writes one classic script for a page. Without `--out` that file is `script.js` in the output directory. `--entry`, `--out`, and `--foreign` override the entry module, the script path, and the foreign directory for that one run. They are not written back. The entry module's `main` of scheme `( -- Unit )` is the root, and the script calls `main()`. A module with no `main` and at least one place roots the script at those places and does not call `main`. The bindings that root reaches are inlined.

```text
plylang check [dir]
plylang build [dir] [--exports [--all] [--check] [<module>...]] [--watch] [--entry <module>] [--out <dir>] [--foreign <dir>]
plylang compile [dir] [--entry <module>] [--out <file>] [--foreign <dir>]
plylang test [pattern]
plylang docs [dir] [--out <file>]
plylang check --parse <file>
plylang check --js <file>
```

`plylang check` checks every file under the source directory, the imports, `foreign`, and each `?` check. It writes no files. The default directory is the current one. `plylang build` checks the modules and writes them into the output directory. It leaves each `?` check out of that typecheck and out of both files. An import is written when the built program uses that name, or when `@` re-exports it. A `.js` file is written when the module has a JavaScript export, `main`, a place, or is `ply.entry`. A `.d.ply` file is written when `@` names something. A file with an empty `@` and no `main`, place, or entry writes nothing, and a file already at that path is removed. `build` does not change `exports`. `plylang build --exports` refreshes the `ply` and `import` paths of modules already listed in `exports`. A source file is added when its name is passed, as in `plylang build --exports Data/Sheet`, or when `--all` is passed. `--all` adds each source module whose `@` names something. Several names may follow `--exports`. A name that is not a source file is an error, and this run writes nothing. Names that stay out of the list are printed under `not exported`. `plylang build --exports --check` prints that report and leaves `package.json` unchanged. The modules are still written. The exit code is 1 when a path would change, an entry names a module that is gone, or `--all` would add a module whose `@` names something. `not exported` by itself leaves the exit code at 0. `--all` together with a module name is rejected. `--watch` cannot be combined with `--exports`. A module whose file is gone is removed when its entry has a `ply` field. A key that does not name a module of this package stays as written. On a module that stays, the `ply` and `import` strings are updated and the other fields stay. A module with no `.js` keeps `ply` and has no `import`. `.` follows `ply.entry` from the file on disk when that module is listed or is being added. With no `ply.entry`, a hand-written `.` stays. Each written path uses `ply.out` from that file. `--entry`, `--out`, and `--foreign` apply to this run and stay out of `package.json`. A module of this package is imported by its path under the source directory, whether or not `exports` lists it. Another package finds the module through the `exports` entry.

A check stands in column 0.

```ply
? "seed stays" < 0 > [
  < 2 10 Occurrence > streamFrom
  0 stepper
  0 runAt
  1 valueAt
]
```

The name is a string literal. The picture is the stack from the bottom upward. The quotation is the program, and it starts from an empty stack. `< 1 2 >` is two values. `<>` is an empty stack. The quotation calls words of this file, including a word that stays off the `@` list. A word the check alone needs is written inside the quotation. The check is not a published name.

`plylang test` builds the package, then finds `?` in source files that match one pattern. The pattern is relative to the source directory. The default is `**/*.ply`. `*` matches inside one path segment. `**` matches across directories. Quote the pattern so the shell does not expand `*`. A file with no `?` is left out. The runner typechecks each check and runs it. The comparison is structural. A failure prints the name, `file:line`, and both stack pictures. One failed check still lets the other checks in that file run. A throw is a failure of that named check. One file that does not compile still lets the other files run. The runner writes the check into a temporary directory inside the package and deletes that directory when the run ends, including when a check fails to compile. The modules in the output directory stay, and they contain no check. A source file cannot import a package that is only a `devDependency`. A failed check, a compile error, or a throw exits 1. No file matched exits 1 with `no tests matched`. Color is used when stdout is a terminal and `NO_COLOR` is unset.

A word named `main` whose scheme is `( -- Unit )` is the program of that module. It may stay off the `@` list. `build` calls it at the bottom of that module's `.js`, and only when Node started that file. `node dist/App.js` runs the program. An import of the module does not. A `package.json` script can name that file. Several modules may each have a `main`. A module without one is a library. Any other scheme on the name `main` is an error. `plylang compile` also writes those modules, then the page script. `--out` names that file. Otherwise it is `script.js` beside the modules. `--entry` names the module for this run, instead of `ply.entry`. The root is `main` when that module defines `main` of scheme `( -- Unit )`, and the script calls `main()`. A module with no `main` and at least one place roots the script at those places, and the script does not call `main`. A module with neither is an error. A binding is inlined when the root can call it. The modules themselves still carry the Node guard. `compile` does not rewrite `exports`. `--foreign` names the foreign directory for this run. A relative `--out` or `--foreign` stays inside the package.

`plylang docs` writes `DOCS.md` at the package root. `--out` names another file. A module heading is the string used to import it: the package name and the path, or only the path when there is no name. The prose comes from `..`. Inside a module the sections are `Types`, `Classes`, `Instances`, and `Words`. An empty section is left out. The name of a type, a class, or a word is a heading under its section. A method is a heading after its class. The class heading carries the class text. The method heading carries that method's text. `Instances` is a list of `#` heads, with no comment and no method bindings. A head whose type is defined in another module of this package is listed there too.

`plylang check --parse` prints how one file was parsed, and it does not check types. `plylang check --js` prints the JavaScript that `build` would write for that file. Outside a package that is the JavaScript of the one file, and imports are not resolved.

The exit code is 0 for success, 1 for a diagnostic, and 2 for bad arguments. `--help` prints the same lines. Messages are in English: `file:line:column: error: PLxxxx: message`, and the next line is a link to the matching section of `docs/Errors.md`.

What enters a word's function is its own prefix, from the bottom upward. One result is returned by itself. Several results come back as a JavaScript array: that is the calling convention, and it is not an `(Array a)` value. The words from `dup` through `over2` do not become calls. `call`, `dip`, `keep`, `bi`, `if`, and their neighbors inline the body of the quotation. `cons` builds a quotation that pushes the value under it and then runs it.

## Building

From this repository:

```text
cargo build --release
```

`target/release/plylang` and `target/release/plylang-ls` are the compiler and the server. The server speaks LSP over stdio. In a package with a `ply` field it resolves imports the same way `plylang build` does.

`@plylang/plylang` installs the `plylang` command. `@plylang/language-server` installs `plylang-ls` and depends on the same version of the compiler. Publish the compiler and its six platform packages before the server package and its six platform packages. `.github/workflows/publish.yml` does that when a GitHub release is published. Version `1.x` is language `1`. A minor or a patch of the compiler does not change the language number. `PLYLANG_BIN` and `PLYLANG_LS_BIN` supply a binary of their own.

`node script/stage-native.js <platform> <binary-dir> <out-dir> plylang` writes one compiler platform package. The same command with `plylang-ls` writes `@plylang/language-server-<os>-<arch>`. Each archive holds one binary.

Highlighting is the [tree-sitter-ply](https://github.com/algosail/tree-sitter-ply) grammar. The Zed extension is the neighboring repository `zed-ply`. It installs `@plylang/language-server` for the editor. In this repository, `cargo build -p plylang-ls` is what the extension runs.

The license is MIT.
