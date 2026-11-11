# The shared expression language

Version 1. Status: specification for WP-UI-11 (blueprint WS9, ADR-28). The Rust implementation
(`spot-expr`) and the .NET implementation must give **identical results** on every case of the
conformance corpus (`crates/framework/spot-expr/corpus/`). This document is the whole
specification: a second implementation needs nothing else.

The language is small on purpose. It is evaluated on the client for instant field rules
(depends-on, mandatory-if, read-only-if, link filters, defaults) and on the server for the same
rules. It is **pure and total**: no clocks, no randomness, no I/O, no loops, no recursion, no
user-defined functions. Every expression terminates.

## 1. Values and types

| Type | Meaning | Notes |
| --- | --- | --- |
| `null` | no value | |
| `bool` | `true` / `false` | |
| `int` | 64-bit signed integer | checked arithmetic |
| `decimal` | exact decimal | at most 28 significant digits, scale 0 to 28; **no floating point anywhere** |
| `text` | a sequence of Unicode scalar values | |
| `date` | calendar date, years 0001 to 9999 | proleptic Gregorian |
| `datetime` | an instant in UTC, microsecond precision, years 0001 to 9999 | |
| `list` | an ordered list of values | elements may have different types; lists nest |

A decimal carries a **scale** (the number of digits after the point): `12.50` has scale 2 and
`12.5` scale 1. Two decimals are *equal* when their numeric values are equal, whatever their
scales; the scale is part of the result and is observable through `text(...)` and the corpus.

`int` and `decimal` are the **numeric** types. In mixed arithmetic an `int` is converted to a
decimal of scale 0.

**Decimal precision, once:** a decimal has at most 28 significant digits. Intermediate results are computed in signed 128-bit integers; an operation whose exact intermediate does not fit in 128 bits, or whose result has more than 28 significant digits, is `expr.overflow`. (Division scales the dividend by `10^(t+scale(b))` and the divisor by `10^scale(a)`, with `t` the result scale.)

## 2. Lexical rules

Source text is Unicode. Positions in errors are **character offsets** (0-based, counting Unicode
scalar values) of the offending token's first character.

- **Whitespace**: space, tab, line feed, carriage return. Separates tokens; otherwise ignored.
- **Identifiers**: `[A-Za-z_][A-Za-z0-9_]*`. Case-sensitive.
- **Keywords**: `and`, `or`, `not`, `in`, `true`, `false`, `null`. Keywords are lower-case only.
- **Integer literal**: `[0-9]+`. Must fit in `i64` (at most 9223372036854775807), else
  `expr.number_range`. There is no negative literal; `-5` is the unary minus applied to `5`.
  (So `-9223372036854775808` cannot be written; `-9223372036854775807 - 1` can.) Leading zeros
  are allowed and ignored.
- **Decimal literal**: `[0-9]+ "." [0-9]+`. The scale is the number of digits after the point,
  trailing zeros included (`1.50` has scale 2). More than 28 significant digits or a scale above
  28 is `expr.number_range`. A bare `1.` or `.5` is a syntax error.
- **Text literal**: `'...'` or `"..."`. Escapes: `\\`, `\'`, `\"`, `\n`, `\t`, `\r`. Any other
  escape is `expr.bad_escape`, reported at the offset of the backslash. A raw line feed inside the literal is allowed. An unterminated
  literal is `expr.unterminated_string`.
- **Punctuation**: `( ) [ ] , . ? :`
- **Operators**: `+ - * / % == != < <= > >=`
- Any other character is `expr.syntax`.

## 3. Grammar

PEG, whitespace between tokens omitted for readability. `/` is ordered choice.

```
expression  <- ternary EOF
ternary     <- or ( "?" ternary ":" ternary )?
or          <- and ( "or" and )*
and         <- not ( "and" not )*
not         <- "not" not / comparison
comparison  <- additive ( cmp_op additive )?          # at most one: not associative
cmp_op      <- "==" / "!=" / "<=" / ">=" / "<" / ">" / "in"
additive    <- multiplicative ( ("+" / "-") multiplicative )*
multiplicative <- unary ( ("*" / "/" / "%") unary )*
unary       <- "-" unary / primary
primary     <- literal / list / call / path / "(" ternary ")"
literal     <- INT / DECIMAL / TEXT / "true" / "false" / "null"
list        <- "[" ( ternary ( "," ternary )* )? "]"
call        <- IDENT "(" ( ternary ( "," ternary )* )? ")"
path        <- ("doc" / "parent") ( "." IDENT )+
```

`a < b < c` is a syntax error (`expr.syntax`), as is a trailing comma. A bare identifier that is
not a keyword, not followed by `(`, and not a path root is `expr.unknown_name`. A path whose root
is not `doc` or `parent`, or that is just `doc`, is also `expr.unknown_name`.

### Precedence, from loosest to tightest

| Level | Operators | Associativity |
| --- | --- | --- |
| 1 | `? :` | right |
| 2 | `or` | left |
| 3 | `and` | left |
| 4 | `not` | prefix |
| 5 | `== != < <= > >= in` | none |
| 6 | `+ -` | left |
| 7 | `* / %` | left |
| 8 | unary `-` | prefix |

So `not a == b` is `not (a == b)`, and `-a * b` is `(-a) * b`.

### Limits (checked at parse time)

- **Node count**: at most **2000** nodes. Every literal, list, path, call, unary, binary and
  ternary node counts as one (parentheses are not nodes). More is `expr.too_complex`.
- **Nesting depth**: at most **64**. Depth is the length of the longest path from the root to a
  node, the root being depth 1. More is `expr.too_deep`.

Nodes are created in post-order (a node when its last child is complete); both limits are checked at that moment, so a left-deep chain `1 + 1 + ... + 1` of n terms has depth n. Both limits are checked while parsing, so an over-limit expression never reaches evaluation.
Which of two simultaneous problems is reported is defined by the order of checking: the parser
reads left to right and reports the **first** problem it meets; the node limit is checked when a
node is created and the depth limit when a node is entered.

### Names resolved at parse time

A call to a function that is not in the library is `expr.unknown_function`; a call with a
number of arguments the function does not accept is `expr.arity`. Both are parse errors, reported
at the position of the function name.

## 4. Fields

`doc.qty` reads the field `qty` of the document being edited; `parent.status` reads a field of
the parent document (for a child row). A longer path (`doc.customer.name`) reads through
links as the host defines. The host supplies a **context** with one operation: *look up a path*
(the names after the root, in order, plus which root) which returns a value or "nothing". A
path with no value is **`null`**; it is never an error.

The context also supplies **`now`**, an instant, used only by the function `now()`. Nothing else
in the language reads the outside world.

## 5. Null

`null` is an ordinary value with these rules, stated once:

- **Equality**: `null == null` is `true`; `null == x` is `false` for any non-null `x`; `!=` is
  the negation of `==`.
- **Ordering** (`< <= > >=`): if either operand is `null`, the result is **`false`**. (Not an
  error and not "unknown".)
- **Arithmetic** (`+ - * / %`, unary `-`): if either operand is `null`, the result is `null`.
  This is checked **before** the type check: `null + 'a'` is `null`.
- **Logic**: `and`, `or`, `not` and the condition of `? :` treat `null` as `false`.
- **`in`**: `x in null` is `false`; `null in [null]` is `true` (list membership uses `==`).
- **Functions**: each function states its own rule (section 8). Unless stated, a `null`
  argument makes the result `null`.

There is no three-valued logic: a condition is either true or false.

## 6. Operators

### 6.1 Logic

`a and b`: if `a` is not true (false or null) the result is `false` and `b` is **not
evaluated**; otherwise the result is the truth of `b`. `a or b`: if `a` is true the result is
`true` and `b` is not evaluated; otherwise the truth of `b`. `not a` is the negated truth.
The operand of a logic operator must be `bool` or `null`; any other type is `expr.type`.
The result is always a `bool`.

`c ? x : y` evaluates `c` (same typing and null rule), then only the chosen branch.

Because of short-circuiting, an error in an unevaluated operand or branch never happens.

### 6.2 Comparison

- `==` / `!=`: values of different types are unequal, except that `int` and `decimal` compare
  by numeric value (`1 == 1.00` is `true`). `date` and `datetime` are different types and never
  equal. Lists are equal when they have the same length and equal elements pairwise (using `==`).
  Text equality is exact scalar-by-scalar (no case folding, no normalisation).
- `<`, `<=`, `>`, `>=`: both operands must be numeric, both `text`, both `date`, or both
  `datetime`; any other combination of non-null operands is `expr.type`. Text is ordered by
  Unicode scalar value (not by locale). With a `null` operand the result is `false`.
- `x in list`: `true` if some element equals `x` under `==`. The right operand must be a `list`
  or `null`, else `expr.type`.

### 6.3 Arithmetic

Operands are numeric (after the null rule); otherwise `expr.type`, with one extension:
`text + text` is concatenation.

Let the **scale** of an `int` be 0 and of a `decimal` its scale. Let `s` be the larger scale of
the two operands.

| Operation | Operands | Result |
| --- | --- | --- |
| `a + b`, `a - b` | `int`, `int` | `int`, checked (`expr.overflow`) |
| | otherwise numeric | `decimal`, exact, scale `s` |
| `a * b` | `int`, `int` | `int`, checked |
| | otherwise numeric | `decimal`: the exact product rounded **half-even** to scale `s` |
| `a / b` | numeric | `decimal`: the exact quotient rounded half-even to scale `max(s, 6)` |
| `a % b` | `int`, `int` | `int`, the remainder with the sign of `a` |
| | otherwise numeric | `decimal`, the remainder with the sign of `a`, scale `s` |
| `-a` | `int` | `int`, checked |
| | `decimal` | `decimal`, same scale |

**The rounding rule, once:** whenever the language rounds a decimal it rounds **half to even**
(banker's rounding: a tie goes to the neighbour whose last digit is even). The target scale is
the scale of the widest operand (`s`), and for division at least 6.

Division and remainder by zero (an `int` zero, or a decimal equal to zero) are
`expr.division_by_zero`: there is no infinity and no NaN. A decimal result that does not fit
(more than 28 significant digits after rounding) is `expr.overflow`.

`text + text` concatenates. `text + int` is `expr.type` (use `text(x)`).

## 7. Dates and times

- Text for a `date` is `YYYY-MM-DD` (four-digit year, two-digit month and day, a real calendar
  day). Text for a `datetime` is `YYYY-MM-DDTHH:MM:SS` optionally followed by `.` and one to six
  fraction digits, then `Z` (UTC only). Anything else is `expr.bad_date`.
- Canonical output (`text(x)`): a date as `YYYY-MM-DD`; a datetime as `YYYY-MM-DDTHH:MM:SSZ`,
  or `YYYY-MM-DDTHH:MM:SS.ffffffZ` (always six digits) when the microsecond part is not zero.
- `date_add` moves by whole days. A result outside year 0001 to 9999 is `expr.overflow`.

## 8. Function library

Names are lower-case. Arity is checked at parse time. Argument types are checked at evaluation
(`expr.type`). "Null rule" says what a `null` argument does; "otherwise null" means the result
is `null` when any argument is `null`.

| Function | Result | Rule |
| --- | --- | --- |
| `len(x)` | `int` | length of a `text` in Unicode scalar values, or of a `list`; `len(null)` is `0` |
| `lower(t)` | `text` | lower-case (Unicode simple case mapping); otherwise null |
| `upper(t)` | `text` | upper-case (Unicode simple case mapping); otherwise null |
| `trim(t)` | `text` | removes leading and trailing whitespace (the lexical whitespace characters plus U+00A0); otherwise null |
| `contains(x, y)` | `bool` | `x` text and `y` text: substring test (the empty text is always contained); `x` list: some element `== y`; `contains(null, y)` is `false` |
| `starts_with(t, p)` | `bool` | text prefix; a null argument gives `false` |
| `ends_with(t, p)` | `bool` | text suffix; a null argument gives `false` |
| `substr(t, start, count)` | `text` | `count` scalar values from 0-based `start`, clipped to the text; a negative `start` or `count` is `expr.type`; otherwise null |
| `replace(t, from, to)` | `text` | replaces every non-overlapping occurrence of `from` (left to right); an empty `from` is `expr.type`; otherwise null |
| `is_empty(x)` | `bool` | `true` for `null`, the empty text and the empty list; `false` for everything else |
| `coalesce(a, b, ...)` | any | the first argument that is not `null`, else `null`; at least one argument; all arguments are evaluated |
| `round(x, n)` | `decimal` | `x` numeric, `n` an `int` from 0 to 28: round half-even to scale `n`; the result has scale exactly `n`; otherwise null |
| `abs(x)` | numeric | absolute value; `int` stays `int` (checked); otherwise null |
| `min(a, b, ...)` | numeric or other | the least of at least two arguments of one comparable kind (all numeric, all text, all date, or all datetime); a null argument gives `null`; the result keeps the type and scale of the chosen argument; on a tie the first is chosen |
| `max(a, b, ...)` | as `min` | the greatest; on a tie the first |
| `int(x)` | `int` | `int` unchanged; `decimal` truncated toward zero (`expr.overflow` if out of range); `text` parsed as an optionally signed integer (`expr.bad_number` if it is not one, `expr.overflow` if it does not fit); otherwise null |
| `decimal(x)` | `decimal` | `decimal` unchanged; `int` as scale 0; `text` parsed as an optionally signed decimal literal with the lexical rules of section 2, no exponent (`expr.bad_number` otherwise; too many digits is `expr.overflow`); otherwise null |
| `text(x)` | `text` | canonical text: `null` becomes the empty text; `true`/`false`; an `int` in decimal digits; a decimal with its scale; a text unchanged; a date or datetime as in section 7; a list as `[` elements `, `-separated `]`, where inside a list `null` is written `null` and text elements are quoted with `"` and `\`-escaped as in section 2 |
| `date(x)` | `date` | `date` unchanged; `datetime` to its UTC date; `text` parsed (`expr.bad_date` otherwise); otherwise null |
| `datetime(x)` | `datetime` | `datetime` unchanged; `date` as midnight UTC; `text` parsed (`expr.bad_date` otherwise); otherwise null |
| `now()` | `datetime` | the context's `now`, truncated to microseconds |
| `date_add(d, n)` | `date` | `d` a date, `n` an int: `d` plus `n` days (`n` may be negative); otherwise null |
| `days_between(a, b)` | `int` | `b - a` in whole days, for two dates; a datetime argument is converted to its UTC date first; otherwise null |
| `year(d)` | `int` | the year of a date (or of a datetime's UTC date); otherwise null |
| `month(d)` | `int` | the month 1 to 12; otherwise null |
| `day(d)` | `int` | the day of the month 1 to 31; otherwise null |

There is deliberately no `today`: an expression that needs today's date writes
`date(now())`, and the host decides what `now` is.

## 9. Errors

Parse errors carry the code, a message and a character offset. Evaluation errors carry the
code and a message (no position). **Codes are the contract; messages are not.** The first error
met during evaluation, in the evaluation order below, is the one reported.

| Code | When |
| --- | --- |
| `expr.syntax` | the text does not match the grammar, or an unexpected character |
| `expr.unterminated_string` | a text literal is not closed |
| `expr.bad_escape` | an unknown escape in a text literal |
| `expr.number_range` | a numeric literal does not fit its type |
| `expr.too_complex` | more than 2000 nodes |
| `expr.too_deep` | nesting deeper than 64 |
| `expr.unknown_name` | an identifier that is not a keyword, call or path root |
| `expr.unknown_function` | a call to a function not in the library |
| `expr.arity` | a call with an unacceptable number of arguments |
| `expr.type` | an operand or argument of the wrong type, or an invalid argument value where section 8 says so |
| `expr.division_by_zero` | `/` or `%` by zero |
| `expr.overflow` | an `int`, `decimal` or `date` result out of range |
| `expr.bad_date` | text that is not a date or datetime |
| `expr.bad_number` | text that is not an integer or decimal |

### Evaluation order

Operands are evaluated **left to right**, arguments left to right, list elements left to right.
`and`, `or` and `? :` short-circuit as in section 6.1. All other operators and all functions
evaluate every operand first, then check types, then compute. For a binary operator the type
check happens after the null rule (section 5).

## 10. Canonical text

An implementation can print an expression in a canonical form that parses back to an equal
tree: single spaces around binary operators and after commas, the minimum parentheses that
the precedence table requires, text literals in double quotes with `\\ \" \n \t \r`
escapes, decimals with their scale. The canonical form is not part of the conformance corpus
(it is an aid for tooling), but `parse(print(parse(s))) == parse(s)` must hold.

## 11. The conformance corpus

`crates/framework/spot-expr/corpus/*.json` is the executable form of this document. See its
`README.md` for the file format. Every case gives an expression, a context, a `now` and either a
typed result or an error code; results are compared **by type, value and (for decimals) scale**.
A change to a case needs a change to this document first.
