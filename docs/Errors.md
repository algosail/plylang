# Errors

`plylang` prints a diagnostic on two lines:

```
file:line:column: error: PLxxxx: message
https://github.com/algosail/plylang/blob/main/docs/Errors.md#plxxxx
```

The line and the column start at 1. The column counts Unicode scalar values. The message is English. The language server publishes the same text, and sets `code` and `codeDescription.href` to the same code and link.

A file or directory failure has no line. It prints `path: error: PLxxxx: message` and the same link.

**Unclassified**

## PL0000

`PL0000`

The compiler produced a message that has no code in this list. A debug build stops on that message. A release build prints `PL0000`.

Fix: treat the message as a compiler bug. Report the full text.

**Lexer**

## PL0001

`unexpected character`

The character is not part of Ply. A field insert is `+` glued to a name (`+city`). A space, a second `+`, or a mark such as `*` or `@` is this error.

Fix: delete the character or rewrite the token. The stack word for Factor's `-rot` is `unrot`.

## PL0002

`unterminated string`

A `"` was opened and the line or the file ended before the closing `"`.

Fix: close the string. A newline inside a string is written `\n`.

## PL0003

`unknown string escape`

`\` was followed by a character other than `\`, `"`, `n`, `t`, or `r`.

Fix: use one of those escapes, or write the character itself.

## PL0004

`stray '-'`

`-` is a type synonym only at column 0. An indented `-` is not a minus operator and not a word.

Fix: start the synonym at column 0 (`- Age = Num`), or delete the sign.

## PL0005

`expected exponent digits`

A numeric literal has `e` or `E` and no digits after it (`1e`, `1.0e+`).

Fix: write the exponent (`1e2`) or drop the `e`.

## PL0006

`type variable must start with a lowercase letter`

A stack tail `..name` must be a lowercase type variable. `..S` is this error.

Fix: write `..s`.

**Layout**

## PL0007

`expected language directive`

An interface file (`.d.ply`) opens with `language` and a version.

Fix: put `language 1` on the first line.

## PL0008

`language directive is only valid in an interface`

`language` belongs at the top of a `.d.ply` file. A `.ply` source has no language line.

Fix: delete the directive from the source file.

## PL0009

`expected language version`

`language` was not followed by a version number.

Fix: write `language 1`.

## PL0010

`language version N is newer than this compiler (M)`

The interface, or `package.json` `"ply.language"`, asks for a language version this compiler does not implement.

Fix: use a compiler that supports that version, or set the version to `1`.

## PL0011

`doc comment must sit above a declaration`

A column-0 `..` block belongs on the next `:`, `;`, `-`, or `&`. It is sitting on an import or on something that is not a declaration.

Fix: move the block so it touches that declaration, with a single newline between them. A `.` line comment can sit anywhere.

## PL0012

`import must come before declarations`

Every `~` import is written before the first declaration.

Fix: move the import above the declarations.

## PL0013

`declaration must start at column 0`

`:`, `;`, `-`, `&`, and `#` start at column 0. An indented line is still part of the previous body.

Fix: move the declaration to column 0.

## PL0014

`expected declaration`

The parser expected an import or a declaration. A word starts with `:`, a data type or newtype with `;`, a synonym with `-`, a class with `&`, an instance with `#`, and an import with `~`. The name `def` is an ordinary identifier.

Fix: start the line with one of those marks. `: name` is a word. `def : name` is this error.

**Parser**

## PL0015

`expected import path`

`~` must be followed by a string path.

Fix: write `~ "Data/Maybe" (fromMaybe)`.

## PL0016

`expected imported name`

The parenthesized list after an import path is missing a name, or a name is not an identifier.

Fix: list the names you import: `~ "Data/Maybe" (Maybe, fromMaybe)`.

## PL0017

`expected '('`

A signature, an import list, a constructor field list, or a grouped type needed `(`.

Fix: open the parenthesis where the form requires it. A word signature is `: name ( Num -- Num )`.

## PL0018

`expected ')'`

A `(` was not closed.

Fix: add the matching `)`.

## PL0019

`expected type name`

A type position needed an uppercase constructor name.

Fix: write the type name (`Maybe`), or a lowercase variable where a variable is allowed.

## PL0020

`expected type variable`

A type parameter, a quantified binder, or a class parameter must be a lowercase name.

Fix: write `a`, not a type constructor, in that position.

## PL0021

`expected '='`

A data constructor list, a synonym, or an instance method needed `=`.

Fix: write `=`. A newtype uses `==` once, on the type: `; Age == Years Num`.

## PL0022

`expected constructor`

`;` introduced a data type or newtype and no constructor name followed.

Fix: write at least one constructor: `; Maybe a = Nothing | Just a`.

## PL0023

`newtype expects one constructor`

`==` defines a newtype, which has one constructor.

Fix: keep a single constructor, or use `=` when the type has several.

## PL0024

`newtype expects one field`

That constructor has one field. The JavaScript value is the field itself.

Fix: give the constructor one field: `; Age == Mk Num`.

## PL0025

`duplicate type variable NAME`

The same type variable is listed twice on one type, class, or scheme.

Fix: keep one occurrence.

## PL0026

`expected definition name`

`:` was not followed by the word's name. `foreign` and `private` may each appear once, in either order, and then the name.

Fix: write `: name`, `: foreign name`, or `: foreign private name`. A repeated modifier is this error.

## PL0027

`expected ':'`

A word declaration, a record field, or a record-pattern field needed `:`.

Fix: write the colon. A record field is `name: value`. A record type field is `name : Type`.

## PL0028

`foreign declaration has a body`

A `foreign` word is implemented by a JavaScript export. It has a signature and no body.

Fix: end the declaration at the signature. Put the function in `foreign/{module}.js`.

## PL0029

`interface declaration has a body`

A `.d.ply` file lists signatures. It does not contain bodies.

Fix: delete the body. The implementation stays in the `.ply` source.

## PL0030

`missing body`

A source word that is not `foreign` has a signature and no body.

Fix: indent the body under the signature, or mark the word `foreign` when the implementation is JavaScript.

## PL0031

`expected term`

A body, a quotation, or the right-hand side of `->` needed a term.

Fix: write the term. An arrow with nothing after it, or a quotation that is only `[`, is this error.

## PL0032

`expected '->'`

An equation or a quotation binder listed patterns or names and never wrote `->`.

Fix: put `->` between the patterns and the body: `Nothing ->` or `x ->`.

## PL0033

`expected pattern`

An equation has no pattern, or a constructor field or a record-pattern slot is empty.

Fix: write a pattern. The rightmost pattern is the top of the stack. `_` discards one value. Parentheses keep one slot: `Just (x)`.

## PL0034

`expected N pattern` or `expected N patterns`

Every equation of one word has the same number of patterns as the first equation.

Fix: give this equation that many patterns. One pattern is `pattern`; any other count is `patterns`.

## PL0035

`expected field name`

A record type, a record literal, or a record pattern needed a field label.

Fix: write the label. A tail is `| r` or `| Type` after the fields, and a bare `|` is not a label.

## PL0036

`expected '}'`

A `{` was not closed.

Fix: add the matching `}`.

## PL0037

`duplicate field name`

The same label appears twice in one record type, record literal, or record pattern.

Fix: keep one field of that name. Replacing a value at runtime is `=name`, which is a different message when the label is already in the row (`PL0060`).

## PL0038

`expected class argument`

A constraint or a superclass was written without a type argument.

Fix: pass the arguments the class declares: `Equal a`, not `Equal`.

## PL0039

`expected class name`

`&`, `#`, or a constraint needed an uppercase class name.

Fix: write the class name. Several classes on one instance are separated by commas: `# Equal, Sortable Int`.

## PL0040

`expected method name`

An instance method binding needed the method's name before `=`.

Fix: write `eq = eqNum` under the instance head.

## PL0041

`expected word`

The right-hand side of an instance binding is the name of a word.

Fix: write that name: `eq = eqNum`.

## PL0042

`expected '=>'`

A constraint list or a superclass list ends with `=>`.

Fix: write `Equal a =>` before the stack, or `& Equal a => Comparable a` before the class being declared.

## PL0043

`expected '.'`

Quantified variables at the start of a scheme are lowercase names ended by `.`.

Fix: finish the list with a dot: `a b. Equal a => a a -- Bool`. A name after another name, with no dot, is this error.

## PL0044

`expected '--'`

A signature or a parenthesized function type needs `--` between the input stack and the output stack.

Fix: write `( Num Num -- Num )`. A parenthesized type that is not a function type lists arguments and then `)`.

## PL0045

`stack tail must be the bottom`

`..s` is the stack under the named slots, so it is written first on that side. A second tail on the same side is the same error.

Fix: write `..s Num -- ..s Num`. `Num ..s` is this error.

## PL0046

`expected type`

A type atom was required: an uppercase name, a lowercase variable, a parenthesized type, or a record type.

Fix: write one of those.

## PL0047

`expected row variable`

The record tail after `|` must be a lowercase row variable, a type, or a record type.

Fix: write `| r`, `| Person`, `| (Person a)`, or `| { name : Str }`.

## PL0048

`expected element`

An array slot or a record-field value was required and the next token is not one.

Fix: put a literal, a name, or another element in the slot. Arrays are `<1, 2>`. Record literals are `{ name: "Ada" }`.

## PL0049

`expected name`

A quotation binder needed a lowercase name before `->`.

Fix: write `x y ->`. The leftmost name is the deepest stack value.

## PL0050

`unterminated array`

`<` was not closed with `>`.

Fix: close the array.

## PL0051

`unterminated quotation`

`[` was not closed with `]`.

Fix: close the quotation.

## PL0052

`type constructor must start with an uppercase letter`

A type constructor starts with an uppercase letter.

Fix: rename `maybe` to `Maybe`. A lowercase name in a type is a variable.

## PL0053

`duplicate definition NAME`

The name is already used in this scope. In one module, words and constructors share a set, types share a set, and classes share a set. A repeated method, a repeated binder, and a superclass listed twice use this same message.

Fix: rename one of the two definitions.

**Checker**

## PL0054

`undefined word NAME`

The name is not a compiler word, a word in this module, or a name you imported.

Fix: declare it, import it, or correct the spelling. A `private` word is visible only in its own module.

## PL0055

`undefined type NAME`

The type is not a builtin, a type in this module, or a type you imported.

Fix: declare it, import it, or correct the spelling. Builtin types include `Int`, `Num`, `Str`, `Bool`, `Unit`, `Array`, and `StrMap`.

## PL0056

`cannot redefine builtin NAME`

The name is a compiler word, such as `dup` or `call`.

Fix: choose another name. Those words are in scope without an import.

## PL0057

`cyclic type NAME`

A synonym refers to itself, including through other synonyms.

Fix: break the cycle. A data type or a newtype may mention itself in a field. A synonym may not.

## PL0058

`type variable NAME is not quantified`

The variable is used in a type and is neither a parameter of that type nor bound by the scheme's `a b.` list.

Fix: add it to the type parameters or to the binder list. A free name in a superclass is this error.

## PL0059

`type constructor NAME expects N argument` or `type constructor NAME expects N arguments`

The constructor was given the wrong number of arguments. One argument uses `argument`; any other count uses `arguments`.

Fix: pass that many arguments. `Maybe` takes one: `Maybe a`.

## PL0060

`duplicate field NAME`

The record row already contains this label. This includes `+name` on a record that already has `name`, and a label that appears again after a tail is expanded.

Fix: drop the extra label. To change a value that is already there, use `=name`.

## PL0061

`type mismatch: expected A, found B`

A type, a stack, or a record row does not match. There is no subtyping. `Int` and `Num` are different types. A closed record and a record with another tail are different.

Fix: change the value or the signature so both sides print as the same type.

## PL0062

`type mismatch: expected a record, found TYPE`

The position needs a record. `/name`, `=name`, and `+name` read or write a record. A tail after `|` must also elaborate to a record when it is a type.

Fix: pass a record, or change the tail to a record type.

## PL0063

`type mismatch: expected a data type, found TYPE`

A constructor pattern was checked against a type that has no constructors. The type may be `Int`, a record, a variable, or another non-data type.

Fix: match a value of the data type, or change the pattern.

## PL0064

`stack underflow`

The word takes more values than the stack has. A signature tail is rigid, so the body cannot consume below it.

Fix: add the missing inputs to the signature, or remove the extra consumer. `: bad ( ..s -- ..s )` followed by `drop` is this error.

## PL0065

`occurs check failed`

Unification would build a type that contains itself. `[ dup ] [ drop ] bi` does this: the two quotations do not agree on one result type.

Fix: make every branch produce the same shape. A quotation passed to `bi`, `tri`, `bis`, or `bia` returns one value.

## PL0066

`kind mismatch: expected KIND, found KIND`

A type is used at the wrong kind. `Maybe` has kind `Type -> Type`. A field, a class argument, and a synonym need a kind that matches how the type is applied.

Fix: pass the missing arguments, or remove the extra ones.

## PL0067

`duplicate effect LABEL`

An effect row lists the same label twice. Source programs do not write effect labels.

Fix: if the source does not mention effects, report the diagnostic as a compiler bug.

## PL0068

`effect mismatch: expected ROW, found ROW`

Two effect rows do not match. Source programs do not name effects. A quotation carries an empty effect row.

Fix: if the source does not mention effects, report the diagnostic as a compiler bug.

## PL0069

`effect mismatch: expected { | _ }, found ROW`

A nested effect variable was fixed to a concrete row. It has to stay the open row `{ | _ }`.

Fix: if the source does not mention effects, report the diagnostic as a compiler bug.

## PL0070

`record has no field NAME`

The record type does not have this label. `/name` and `=name` only see labels already in the row. An open tail `| r` does not add a label.

Fix: add the field to the type, or insert it with `+name` on a closed record that does not yet have it.

## PL0071

`record tail must be closed`

`+name`, and a check that extends a closed row, needs a record with no open tail.

Fix: close the row in the signature, or accept extra labels with `| r` instead of inserting one.

## PL0072

`quotation arity is not known`

`call` needs every input type of the quotation to be a solved type. `[ [ dup ] call ] drop` leaves those types unknown.

Fix: call the quotation where the signature already fixes the stack types.

**Patterns**

## PL0073

`redundant pattern`

An earlier equation already covers this one. The first match wins.

Fix: delete the equation, or move a more specific equation above the one that covers it. Two `Nothing` equations are this error.

## PL0074

`non-exhaustive match`

The equations do not cover every value of the matched type, and the checker has no example to print.

Fix: add the missing equation.

## PL0075

`non-exhaustive match: EXAMPLES`

The printed constructors, literals, or shapes are not covered. A `Maybe` matched only with `Nothing` reports `Just`.

Fix: add an equation for each example.

## PL0076

`pattern NAME expects N field` or `pattern NAME expects N fields`

The constructor pattern has the wrong number of fields. One field uses `field`; any other count uses `fields`.

Fix: pass that many fields. `Just` takes one: `Just x`. Parentheses keep one slot when the field is itself a pattern.

## PL0077

`constructor NAME is not a constructor of TYPE`

The pattern names a constructor that does not belong to the type being matched.

Fix: use a constructor of that type. `Left` is not a constructor of `Maybe`.

**Classes and instances**

## PL0078

`undefined class NAME`

The class is not declared in this module and not imported.

Fix: declare it, import it, or correct the spelling.

## PL0079

`class NAME expects a parameter`

A class declaration has no type parameter.

Fix: write the parameter: `& Equal a`.

## PL0080

`class NAME expects N argument` or `class NAME expects N arguments`

A constraint or an instance head passed the wrong number of arguments. One argument uses `argument`; any other count uses `arguments`.

Fix: pass that many types. The count is the number of parameters on the class.

## PL0081

`cyclic superclass: NAME`

The class lists itself as a superclass, directly or through other classes.

Fix: remove that superclass link.

## PL0082

`method NAME is not a method of CLASSES`

The instance binds a name that none of the classes on that head declare. Several classes are separated by commas in the message.

Fix: bind a method those classes declare, or remove the line.

## PL0083

`method NAME is a method of A and B`

More than one class on the same instance head declares this method, so one binding cannot choose an owner.

Fix: split the instance into one head per class, or give the methods different names.

## PL0084

`orphan instance`

This module defines neither the class nor a type constructor in the instance head. A builtin type in the head (`Num`, `Maybe` from the compiler, and the other builtins) counts as defined here.

Fix: move the instance into the module that defines the class, or into the module that defines one of the head types.

## PL0085

`missing method NAME`

The instance does not bind every method of the class.

Fix: add `name = word` for the missing method.

## PL0086

`instance word NAME is private`

The word bound in the instance is declared `: private`. The instance exports that method.

Fix: drop `private` on the word.

## PL0087

`incoherent instance`

Two instances match the same head, a new instance overlaps one already in scope, or two superclass paths reach the same constraint. A synonym is expanded before this check, so two spellings of one type still overlap.

Fix: delete one of the instances, or change a head so the two do not overlap.

## PL0088

`no instance for CONSTRAINT`

No visible instance matches this constraint.

Fix: declare the instance, import the module that declares it, or change the type so an existing instance applies.

## PL0089

`ambiguous constraint CONSTRAINT`

The word bound as a method still has a constraint on a type variable the instance head does not determine. That includes a variable other than the class parameter, and a type the checker has not solved.

Fix: use a word whose extra constraints are fixed by the instance head.

## PL0090

`cyclic instance: KEY`

Solving this instance requires the same instance again. A method context or a superclass instance loops back to the head being declared.

Fix: bind a word that does not demand this instance, or break the superclass cycle that asks for it.

**Modules and packages**

## PL0091

`~ "PATH" does not export NAME`

The imported module does not export that name. The name may be `private`, misspelled, or absent.

Fix: import a public name from that module.

## PL0092

`cannot resolve ~ "PATH"`

The path is not a module of this package and not an export of another package. A missing file is the same message.

Fix: check the spelling. A local module is `Data/Maybe`. A package module is the export path, such as `@algosail/example/Num`. The target must be listed in that package's `exports` with a `ply` condition. `plylang build --exports` writes those entries for the package you are building.

## PL0093

`import cycle: A, B, A`

The named modules import one another in a loop. Names in the message use dots where the path uses slashes.

Fix: move the shared declarations so each import points one way.

## PL0094

`module NAME is not exported`

The package's `exports` has no `ply` condition for this module pointing at `./{out}/{module}.d.ply`. Dots in the message are slashes in the path: `Data.Num` is `Data/Num`.

Fix: add that entry. For this package, `plylang build --exports` rewrites `exports` after a successful build. A check without that flag still requires the entry to exist.

## PL0095

`module segment 'SEGMENT' must start with an uppercase letter`

Each directory and each file name under `ply.source` starts with an uppercase letter.

Fix: rename `data/maybe.ply` to `Data/Maybe.ply`.

## PL0096

`empty module path`

The path has no module name after the source directory.

Fix: give the file a name under `ply.source`, such as `src/Data/Maybe.ply`.

## PL0097

`empty path segment`

The module path contains an empty segment.

Fix: remove the empty segment, such as a doubled separator.

## PL0098

`expected a .ply or .d.ply path`

The path is not a Ply source or a Ply interface.

Fix: point the tool at a `.ply` or `.d.ply` file.

## PL0099

`package.json is not valid JSON`

The package file could not be parsed.

Fix: correct the JSON.

## PL0100

`package.json has no "ply" field`

The compiler reads the `ply` object for the source directory, the output directory, and the language version.

Fix: add `"ply": { "language": 1, "source": "src", "out": "dist" }`.

## PL0101

`package.json ply.source must be a string`

`ply.source` is the directory of `.ply` files, relative to the package root.

Fix: set it to a string such as `"src"`.

## PL0102

`package.json ply.out must be a string`

`ply.out` is the directory where `build` writes `.js` and `.d.ply`.

Fix: set it to a string such as `"dist"`.

## PL0103

`package.json ply.language must be a number`

`ply.language` is the language version. This compiler implements version `1`.

Fix: set `"language": 1`.

## PL0104

`ply.out must be a different directory from ply.source`

The output directory and the source directory are the same, or one of them is empty. The build would treat generated files as source.

Fix: use two different relative directories.

## PL0105

`package.json ply.foreign must be a string`

When `ply.foreign` is present, it is the directory of foreign JavaScript, relative to the package root.

Fix: set it to a string such as `"foreign"`, or remove the field when the package has no foreign words.

## PL0106

`ply.foreign must be a different directory from ply.source and ply.out`

The foreign directory is empty, equal to source or output, or nested inside one of them.

Fix: use a third directory, usually `"foreign"`.

## PL0107

`package.json has no "ply.foreign" field`

A module declares a `foreign` word, and the package does not name a foreign directory.

Fix: add `"foreign": "foreign"` under `ply`.

## PL0108

`foreign word NAME is not exported`

The JavaScript file for this module is missing, or it does not export `NAME`. The file is `foreign/{module}.js`, mirroring the path under `ply.source`.

Fix: export a function with that name. A `private` foreign word is still exported from that file; `private` only hides it from other Ply modules.

## PL0109

`foreign word NAME expects N parameters, found M`

The JavaScript function's parameter count differs from the number of inputs in the Ply signature.

Fix: give the function one parameter per input, or change the signature to match the function.

## PL0110

`package.json must be an object`

`plylang build --exports` rewrites the top-level `exports` field and found a JSON value that is not an object.

Fix: make the top level of `package.json` an object.

## PL0111

`source directory not found`

The `ply.source` directory does not exist. The diagnostic names that directory and has no line or column.

Fix: create the directory, or correct `ply.source`.

## PL0112

`path: OS error`

Reading or writing a file failed. The message is the path and the operating-system error. This covers a source file, an output file, a foreign file, and `package.json` when the failure is the file system rather than the JSON.

Fix: create the missing parent directory, correct the path, or fix the permissions.

**Solver**

## PL0113

`frozen`

While searching for an instance, unification tried to bind a variable the solver had frozen. That failure is normally turned into `PL0088` or `PL0087`.

Fix: if this text is the diagnostic you see, report it as a compiler bug.

## PL0114

`arity`

While searching for an instance, a candidate's head length differed from the constraint's argument count. That failure is normally turned into another instance diagnostic.

Fix: if this text is the diagnostic you see, report it as a compiler bug. An instance head that simply has the wrong number of arguments is `PL0080`.

**Entry**

## PL0115

`word main must have scheme ( -- Unit )`

A word named `main` is the program of its module. The scheme has no constraints and no effects, takes no inputs, and returns `Unit`. `private` is allowed. Any other scheme is this error. A module with no word named `main` is a library module.

Fix: write `: main ( -- Unit )` and a body that returns `Unit`, or rename the word. `( . -- . Unit )` and `( ..s -- ..s Unit )` are the same shape and are accepted.
