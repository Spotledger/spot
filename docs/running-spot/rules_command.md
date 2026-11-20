Part of [Running Spot](../running-spot.md).

# `spot rules`: check, try and explain a rule file

Rules (Datalog over the business graph, `spot-rule-graph`) run inside permission-sensitive features: approver resolution, roll-ups, affected-item analysis. `spot rules` gives the author feedback **before** the rule is installed. Everything here is read-only. A rule only derives: the rule language has no way to write.

A rule set is a file `<app>/rules/<name>.rules` (the set's name is the file stem); the host compiles it when the site starts and on every metadata change. These commands take the file by path.

```
spot rules check   FILE [--site S | --app DIR] [--json]
spot rules eval    FILE --facts FACTS.json --query 'predicate(a, _)' [--site S | --app DIR] [--fuel N] [--json]
spot rules explain FILE [--site S | --app DIR] [--json]
```

Metadata to validate against: `--site S` the site's effective metadata (the installed apps, the changes recorded for the site, and, for `check` only, the customisation records its database holds); `--app DIR` the platform plus that app directory's DocType files (no database at all); neither, the platform alone. `--site` and `--app` cannot be combined. `eval` and `explain` **never open a database connection**: with `--site` they read the site's files only, so a custom field that exists only in the database is seen by `check --site` and not by them.

## `check`

Compiles the file and prints every problem at once, sorted by position, as `FILE:LINE:COL: code: message`, each followed by the one source line it names (cut at 200 characters):

```
$ spot rules check approvals.rules --site mysite
approvals.rules:3:14: rule_graph.unknown_predicate: `employee.boss` names neither an edge type, a field nor a derived predicate
    approver(E, A) :- employee.boss(E, A).
approvals.rules:5:1: rule_graph.unsafe: variable `Y` in the head is not bound by a positive atom
    pair(X, Y) :- employee.manager(X, _).
2 problems
```

Exit 0 when there are none, 2 when there are problems, 1 on usage, 3 when the file or the metadata cannot be read. `--json` prints `{"ok": false, "problems": [{"line", "column", "code", "message"}]}`.

## `eval`

Evaluates a rule over facts held in a JSON file, with no database, and prints the rows exactly as the library returns them (sorted, one tuple per line, cells separated by a tab), then a summary line starting with `#`: the number of rows, the fuel limit and whether it was exhausted.

`--query` is `predicate` (all rows) or `predicate(a, _)`: `_` matches anything, otherwise the argument is a constant (`employee#1`, `42`, `"text"`, `true`, `null`) and only rows equal to it in that column are printed. The number of arguments must equal the predicate's arity (exit 1 otherwise).

`--fuel` is the limit of derived tuples (default 1,000,000, at most ten times that). A program that exceeds it fails with `rule_graph.fuel_exhausted` and prints **no rows**: there is never a partial answer. `now()` in a guard reads the epoch, so a result never depends on the day it is run.

### The fact file

One JSON object, a predicate name to an array of tuples:

```json
{
  "employee.manager":     [["employee#1", "employee#0"], ["employee#2", "employee#1"]],
  "employee.delegate":    [["employee#2", "employee#4"]],
  "employee.can_approve": [["employee#0", true], ["employee#1", false]]
}
```

- An **edge type** takes `[holder, target]`, both records. A **field predicate** (`doctype.field`) takes `[record, value]`.
- A **record** is `"entity#N"` (a small test id; `N` is 0 to 65535) or `"entity:UUID"` (a UUIDv7), or `{"record": "entity#N"}`. Rows print records the same way.
- A **value** is a string (text), an integer, `true`/`false`, `null`, or `{"decimal": "12.50"}` (exact, never a float).
- Facts for a predicate the program does not read are ignored; facts for a derived predicate are an error.

Limits: a file over 50 MiB is refused without being read; more than a million tuples is refused. A fact file that is a symlink to a place outside the working directory is refused. A fact value is never echoed in an error: the message names the predicate and the tuple number.

Exit 0 with rows (also with none), 2 when the rule does not compile, fuel is exhausted, or the facts are refused, 1 on usage, 3 when a file cannot be read.

## `explain`

Prints the derived predicates, their arity and **stratum**, what each reads (an edge, a field or another derived predicate), and marks a dependency that runs through negation (`negated`) or an aggregate (`aggregate`). A rule whose predicate nothing query-able reaches gets a warning (not an error): a **query-able** predicate is a derived one that no other derived predicate reads, and a rule is reachable when such a predicate depends on it, directly or not.

```
approvals.rules: 5 rules, 2 strata
stratum 0
  above/2
    depends on: employee.manager/2 (edge), above/2 (derived)
  has_delegate/1
    depends on: employee.delegate/2 (edge)
stratum 1
  approver/2
    depends on: above/2 (derived), employee.can_approve/2 (field), employee.delegate/2 (edge), has_delegate/1 (derived, negated)
```

The order is deterministic (stratum, then name).

## A worked example: an approver chain

```
has_delegate(E) :- employee.delegate(E, _).
above(E, M) :- employee.manager(E, M).
above(E, M2) :- employee.manager(E, M), above(M, M2).
approver(E, A) :- employee.delegate(E, A).
approver(E, A) :- above(E, A), employee.can_approve(A, true), not has_delegate(E).
```

An employee's approver is their delegate when they have one, otherwise the first manager up the chain who can approve. `spot rules eval approvals.rules --facts facts.json --query 'approver(employee#3, _)'` shows it for one employee without a site. The host evaluates the same text as the requesting user, over only the records that user may read.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | done |
| 1 | usage (a query that does not fit the predicate; `clap` parse errors exit 2 as for every command) |
| 2 | refused: problems in the rule file, fuel exhausted, invalid or oversized facts, an outside symlink |
| 3 | failed: a file or the metadata cannot be read |
