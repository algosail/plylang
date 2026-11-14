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

The character is not part of Ply. `{`, `}`, and `+` are this error. A mark that is not a rune, such as `*` or `%`, is this error.

Fix: delete the character or rewrite the token. A field of a type with one constructor is `name>>` or `>>name`. The stack word for Factor's `-rot` is `unrot`.

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

A column-0 `..` block belongs on the next `:`, `;`, `-`, `&`, or `=`. It is sitting on an import or on something that is not a declaration.

Fix: move the block so it touches that declaration, with a single newline between them. A `.` line comment can sit anywhere.

## PL0012

`import must come before declarations`

Every `~` import is written before the first declaration.

Fix: move the import above the declarations.

## PL0013

`declaration must start at column 0`

`:`, `;`, `-`, `&`, `#`, and `=` start at column 0. An indented line is still part of the previous body. `=name` without a space is a word inside a body, not a declaration.

Fix: move the declaration to column 0.

## PL0014

`expected declaration`

The parser expected an import or a declaration. A word starts with `:`, a data type or newtype with `;`, a synonym with `-`, a class with `&`, an instance with `#`, a place with `=`, and an import with `~`. The name `def` is an ordinary identifier.

Fix: start the line with one of those marks. `: name` is a word. `def : name` is this error.

**Parser**

## PL0015

`expected import path`

`~` must be followed by a string path.

Fix: write `~ "Data/Maybe" fromMaybe`.

## PL0016

`expected imported name`

The parenthesized list after an import path is missing a name, or a name is not an identifier.

Fix: list the names you import: `~ "Data/Maybe" Maybe, fromMaybe`.

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

`:` was not followed by the word's name. `foreign` may appear once, and then the name.

Fix: write `: name` or `: foreign name`. A repeated `foreign` is this error.

## PL0027

`expected ':'`

A word declaration or a constructor field needed `:`.

Fix: write the colon. A constructor field is `name : Type`.

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

An equation has no pattern, or a constructor-pattern slot is empty.

Fix: write a pattern. The rightmost pattern is the top of the stack. `_` discards one value. Parentheses keep one slot: `(Just x)`.

## PL0034

`expected N pattern` or `expected N patterns`

Every equation of one word has the same number of patterns as the first equation.

Fix: give this equation that many patterns. One pattern is `pattern`; any other count is `patterns`.

## PL0035

`expected field name`

A constructor with named fields needed a field label.

Fix: write the label, `name : Type`. A constructor with one field has no label: `Some a`. `|` separates constructors.

## PL0036

`expected '}'`

Source does not produce this message. `{` is an unexpected character (`PL0001`).

Fix: delete `{` and `}`.

## PL0037

`duplicate field name`

The same label appears twice in one constructor.

Fix: keep one field of that name. Replacing a value is `>>name`. A missing field on a type with one constructor is `PL0120`.

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

Fix: write `equal = equalNum` under the instance head.

## PL0041

`expected word`

The right-hand side of an instance binding is the name of a word.

Fix: write that name: `equal = equalNum`.

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

A type atom was required: an uppercase name, a lowercase variable, or a parenthesized type.

Fix: write one of those.

## PL0047

`expected row variable`

Source does not produce this message. `|` separates constructors.

Fix: write `Nothing | Just a`. `{` and `}` are an unexpected character (`PL0001`).

## PL0048

`expected element`

Source does not produce this message. An array is a program between `<` and `>`. `{` and `}` are an unexpected character (`PL0001`).

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

Fix: declare it, import it, or correct the spelling. A name left off the `@` list is visible only in its own file.

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

Source does not produce this message. A constructor that repeats a label is `PL0037`.

Fix: keep one field of that name. To change a value that is already there, use `>>name`.

## PL0061

`type mismatch: expected A, found B`

A type or a stack does not match. There is no subtyping. `Int` and `Num` are different types.

Fix: change the value or the signature so both sides print as the same type.

## PL0062

`type mismatch: expected a record, found TYPE`

Source does not produce this message. `name>>` and `>>name` on a type that is not a data type are `PL0063`. `{`, `}`, and `+` are an unexpected character (`PL0001`).

Fix: read a field of a data type.

## PL0063

`type mismatch: expected a data type, found TYPE`

A constructor pattern, `name>>`, or `>>name` was checked against a type that has no constructors. The type may be `Int`, a type variable, a synonym of a non-data type, or another non-data type.

Fix: use a data type, or match with `->`. A field that is not the same on every constructor is `PL0119`. A missing field is `PL0120`.

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

Source does not produce this message. A type with one constructor that has no field of this name is `PL0120`.

Fix: use the field's name, or match with `->`.

## PL0071

`record tail must be closed`

Source does not produce this message. `{`, `}`, and `+` are an unexpected character (`PL0001`).

Fix: declare the fields on the constructor and replace one with `>>name`.

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

The constructor pattern has the wrong number of fields. One field uses `field`; any other count uses `fields`. `(Name ..)` spills the fields. A short list of field names binds those fields. This error is a wrong count of other patterns.

Fix: pass that many fields. `Just` takes one: `(Just x)`. Parentheses keep one slot when the field is itself a pattern. `(Just ..)` spills that field.

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

This code is not produced. A word bound in `#` stays in its file when it is absent from the `@` list, and the compiler still calls it.

## PL0087

`incoherent instance`

Two instances match the same head, a new instance overlaps one already in scope, or two constraints in one signature each supply the wanted class. Several superclass paths from one constraint are one dictionary, and the checker uses the shortest path. A synonym is expanded before this check, so two spellings of one type still overlap.

Fix: delete one of the instances, or write the wanted class once in the signature.

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

The imported module does not export that name. The name may be misspelled or absent from that module's `@` list.

Fix: import a name from that list.

## PL0092

`cannot resolve ~ "PATH"`

The path is not a module of this package and not an export of another package. A missing file is the same message.

Fix: check the spelling. A local module is `Data/Maybe`. A package module is the export path, such as `@algosail/example/Num`. The target must be listed in that package's `exports` with a `ply` condition. `plylang build --exports` writes those entries for the package you are building when you name the module or pass `--all`.

## PL0093

`import cycle: A, B, A`

The named modules import one another in a loop. Names in the message use dots where the path uses slashes.

Fix: move the shared declarations so each import points one way.

## PL0094

`module NAME is not exported`

A module of this package is the file under `ply.source`. The import does not look at `exports`. Another package still finds a module through that package's `exports`. This code is not reported.

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

The compiler reads the `ply` object. `ply.language` is required. `source`, `out`, and `foreign` may be omitted.

Fix: add `"ply": { "language": 1 }`.

## PL0101

`package.json ply.source must be a string`

When `ply.source` is present, it is the directory of `.ply` files, relative to the package root. When it is absent, that directory is `src`.

Fix: set it to a string such as `"src"`, or remove the field.

## PL0102

`package.json ply.out must be a string`

When `ply.out` is present, it is the directory where `build` writes `.js` and `.d.ply`. When it is absent, that directory is `dist`.

Fix: set it to a string such as `"dist"`, or remove the field.

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

A module declares a `foreign` word, and the package has neither `ply.foreign` nor a `foreign` directory.

Fix: create `foreign`, or add `"foreign": "foreign"` under `ply`.

## PL0108

`foreign word NAME is not exported`

The JavaScript file for this module is missing, or it does not export `NAME`. The file is `foreign/{module}.js`, mirroring the path under `ply.source`.

Fix: export a function with that name. A foreign word left off the `@` list is still imported from that file. The list only hides the name from other Ply modules.

## PL0109

`foreign word NAME expects N parameters, found M`

The JavaScript function's parameter count differs from the number of inputs in the Ply signature.

Fix: give the function one parameter per input, or change the signature to match the function.

## PL0110

`package.json must be an object`

`plylang build` rewrites the top-level `exports` field and found a JSON value that is not an object.

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

A word named `main` is the program of its module. The scheme has no constraints and no effects, takes no inputs, and returns `Unit`. It may stay off the `@` list. Any other scheme is this error. A module with no word named `main` is a library module.

Fix: write `: main ( -- Unit )` and a body that returns `Unit`, or rename the word. `( . -- . Unit )` and `( ..s -- ..s Unit )` are the same shape and are accepted.

**Foreign data**

## PL0116

`foreign type has no constructors`

`; foreign` names a type whose values come from JavaScript. The declaration ends after the name and its parameters. `=`, `==`, or a constructor name after that is this error.

Fix: write `; foreign Hub a`. A type that has constructors is an ordinary `;` declaration.

## PL0117

`a constructor with several fields names each one`

Two or more fields were written as types only, as in `Pair a b`. The kind stays on the left of `=`. Each field on the right has a name and a type.

Fix: write `Pair frs : a, snd : b`. A constructor with one field stays a bare type: `Some a`.

## PL0118

`a single field has no name`

A constructor with one field was written `value : Type`. The name of that field belongs in the equation, not in the declaration.

Fix: write `Some a` or `Effect ( -- a )`.

## PL0119

`field NAME is not the same on every constructor`

`name>>` or `>>name` needs that field on every constructor, with the same type. `None` has no `text`. A field that one constructor has and another lacks is the same error. Two constructors that both have the name, with different types, are the same error.

Fix: read it after that constructor's check, `Sent -> body>>`. A field every constructor has is `name>>` with no match. A quotation does not see the check.

## PL0120

`TYPE has no field NAME`

The type or the narrowed constructor has no field of this name. A one-field constructor such as `Effect` has no `name>>`. `None` has no field `a`.

Fix: use the field's name. Name the one field in the equation, `(Just x)`, or spill it with `(Just ..)`.

## PL0121

`~ "PATH" is a devDependency`

A module under the source directory imported a package that is listed in `devDependencies` and not in `dependencies`, `optionalDependencies`, or `peerDependencies`.

Fix: move the package to `dependencies`.

## PL0122

`test module PATH is already a source module`

This code is not produced. A check is a `?` form in a source file.

## PL0123

`package.json ply.entry must be a string`

`ply.entry` is present and is not a string. It names the module opened by a pathless package import and the `exports` key `.`.

Fix: set it to a module path such as `"Stream"`, or remove the field.

## PL0124

`ply.entry module NAME is not in the source`

`ply.entry` names a module path that is not a file under the source directory.

Fix: name a module that `plylang build` compiles, or remove the field.

## PL0125

`module has no name list`

A module under the source directory has no `@` list. A `?` check is not a published name and does not need one.

Fix: write one `@` list. A bare `@` publishes nothing.

## PL0126

`listed name NAME is not defined`

`NAME` is in the `@` list and is neither declared in this file nor brought in by an import.

Fix: declare it, import it, or remove it from the list.

## PL0127

`package.json ply.target is not a field`

`ply.target` is present. The page script is not a package field.

Fix: remove `ply.target`. `plylang compile` writes the page.

## PL0128

`@ is the module's name list`

This code is not produced. The `@` list may stand anywhere in the file. A second `@` is a duplicate definition.

## PL0129

`plylang compile needs ply.entry and a main of scheme ( -- Unit ) or a place`

`plylang compile` writes one page script. `ply.entry` names that module, or `--entry` names it for one run. A `main` of scheme `( -- Unit )` is the root, and the script calls it. A module with no `main` and at least one place roots the script at those places and does not call `main`. A module with neither is this error.

Fix: set `ply.entry` to that module, or pass `--entry`. Give the module a `main` of scheme `( -- Unit )`, or a place. A `main` with another scheme is PL0115.

## PL0130

`compile cannot load "SPEC"`

The page script could not inline `SPEC`. The graph is the JavaScript this compiler wrote and the foreign files those modules import. A `node:` import, a package without `ply`, a file outside that package, an import cycle, or an unsupported `export` stops `plylang compile`.

Fix: import a Ply package this compiler built, or keep that JavaScript in the foreign file of a Ply package. The modules in the output directory stay separate and keep the Node entry guard.

## PL0131

`public scheme mentions private NAME`

A name in the `@` list, or a constructor or class method that is printed with it, mentions `NAME`. `NAME` is a type or a synonym declared in this file and absent from the `@` list. The interface would name it without a definition.

Fix: list `NAME`, or stop mentioning it in what this module publishes. A type imported from another module is already defined there.

## PL0132

`fields of NAME are private`

`name>>` or `>>name` read a field of `NAME`, and the constructor of `NAME` is not in scope. Listing the type name does not list a constructor whose name differs.

Fix: import the constructor, or use a word this module published for that value. In the file that declares `NAME`, the constructor is in scope.

## PL0133

`import is already complete`

`: Alias` finishes the import. A name after it, or a second form on the same import, has nowhere to go.

Fix: put the names on their own `~` line, or list them instead of `: Alias`.

## PL0134

`place NAME is not ready`

The program under `= NAME` reads `NAME`, writes `=NAME`, binds `=>NAME`, or mentions a place that is declared later. A word that program calls counts the same way when the word reads or writes the place. That program runs once, when the module loads, and this place's frame does not exist until the program has finished. A later place is still empty.

Fix: fill the place from a value that does not use it. A word below the declaration may read and write it.

## PL0135

`no place NAME`

`=NAME` or `=>NAME` names a place, and `NAME` is not one. A place is `= NAME Type` in this file, or a place whose reader this file imports. Importing the reader also opens `=NAME` and `=>NAME`.

Fix: declare the place, or import its reader. `NAME` alone reads it. The glued word writes or binds.

## PL0136

`pattern NAME needs a field name or ..`

`(Page)` names no field. A short pattern that is not a list of field names, such as `(Page id _)` or `(Page (Some x))`, is the same error. `(None)` and `(LT)` stay legal: a constructor with no fields leaves nothing.

Fix: keep the value with `Page`, spill every field with `(Page ..)`, or name the fields, `(Page id paths)`. Do not mix `..` with field names.

## PL0137

`pattern cannot test TYPE`

`(Some (Option Num) value)` asks whether the field is `(Option Num)`. A number, an integer, a string, a boolean, or an array of those is a check the runtime can make. `TYPE` is not one of those, and the field is not already `TYPE`.

Fix: test a shape the runtime can see, `(Some (Array Num) items)`. When the field is already `TYPE`, the name is that value and there is nothing to test.

## PL0138

`path "PATH" must stay inside the package`

`--out` and `--foreign` are paths relative to the package root. `PATH` is absolute, empty, or contains `..`.

Fix: pass a relative path that stays inside the package. `plylang compile --out` is the script file. `plylang build --out` is the module directory.

## PL0139

`check name must be a string`

A `?` check names itself with a string literal. The runner reads that literal before the program runs, so a throw still has a name.

Fix: write `? "name" < picture > [ program ]`.

## PL0140

`expected a stack picture`

A `?` check has a name and then a picture of the stack in `<` and `>`. `<>` is an empty stack. `< 1 2 >` is two values, bottom to top.

Fix: write the picture before the quotation.

## PL0141

`expected a quotation`

A `?` check runs one quotation. The quotation starts from an empty stack and must leave the picture.

Fix: write `[ program ]` after the picture.
