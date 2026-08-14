# ulb Language Grammar (GRAMMAR.md)

Authoritative grammar for the **ulb** build DSL (Uliab Build). This document
is the single source of truth for the language: the hand-written recursive-
descent parser in `ulb-lang` and the `tree-sitter-ulb` grammar must both
conform to it. When this document changes, every repository that implements
literal:the language must be notified.

**Status:** draft for review.

---

## 1. Design goals

1. **Small, closed, declarative.** No classes, generics, lambdas-with-
   receiver, reflection, or arbitrary file I/O from DSL source. Every
   construct maps onto config declarations, `fn` helpers, `convention`
   blocks, or native builtins (`env`, `props`).
2. **Tree-sitter portable.** The grammar avoids constructs that are painful
   for GLR/tree-sitter parsing: there is no significant whitespace, no
   layout-sensitive indentation, no statement terminators, and every
   statement/expression is disambiguated by the *next token* (at most one
   token of lookahead). This keeps the hand-written parser simple and lets
   `tree-sitter-ulb` mirror it mechanically.
3. **Editor-robust by construction.** Newlines carry no meaning, so a file
   that is mid-edit (half a line typed, brace not yet closed) still parses
   into a partial AST plus diagnostics (see §11).
4. **One grammar, four file roles.** `settings.ulb`, `build.ulb`,
   `conventions.ulb`, and `libs.ulb` share a single grammar. Which
   statements are *legal* in which role is enforced outside the parser —
   partly by the evaluator, partly by the tool layer and the plugins a
   module applies (see §10) — the LSP parses any `.ulb` file identically.

---

## 2. Notation

EBNF. Terminals are `UPPERCASE` (lexical tokens) or `"quoted"` (literal
keyword/symbol text). Non-terminals are `lowercase`. Rules:

```
=   definition       ;   end of rule
"x"  literal token
X | Y  alternation
[ X ]  optional
{ X }  zero or more
( X )  grouping
(* ... *)  comment
```

Terminals are separated by arbitrary whitespace and/or comments. Comments
are lexed as tokens (so the LSP can render them) but are invisible to the
parser. **Newlines are insignificant**: they are whitespace like any other.

---

## 3. Lexical structure

```
letter        = "a".."z" | "A".."Z" | "_" ;
digit         = "0".."9" ;

IDENT         = letter { letter | digit } ;
NUMBER        = digit { digit } [ "." digit { digit } ] ;
BOOL          = "true" | "false" ;
STRING        = '"' { string_char | interpolation } '"' ;
string_char   = any character except '"' | "\" | newline
              | "\" ( '"' | "\" | "n" | "t" | "r" ) ;
interpolation = "${" expression "}" ;
line_comment  = "//" { any character except newline } ;
block_comment = "/*" { any character } "*/" ;
```

Lexical notes:

- **Strings** use double quotes only; there is no single-quoted form.
  A string may not contain a raw newline. `\"`, `\\`, `\n`, `\t`, `\r`
  are the only escape sequences.
- **Interpolation** (`${...}`) may contain any expression (§5), including
  nested strings and nested interpolations. The lexer must track
  `${`...`}` nesting so that `"${env("X")}"` lexes as one string token.
  Deeply nested interpolation is legal but discouraged by style.
- **Numbers** are non-negative integers or decimals; a trailing decimal
  point (`37.`) is invalid. There is no unary minus; negative values are
  not expressible (none are needed by the current DSL surface).
- **Comments** do not nest. A `/*` inside a block comment ends it.
- Reserved words cannot be used as `IDENT` (§4).

---

## 4. Reserved words vs contextual identifiers

**Reserved** (cannot appear as `IDENT`; error if used as one):

```
if  else  true  false  convention  fn  task  apply
```

**Contextual** (valid `IDENT`s whose *meaning* is assigned by the evaluator
only inside the relevant block; the parser treats them as plain
identifiers):

```
android  buildTypes  productFlavors  signing  deps  repositories  versions
bundle  plugins  maven  dimension  dependsOn  run  plugin  project  module
lspCompat  namespace  applicationId  compileSdk  minSdk  targetSdk
versionCode  versionName  api  implementation  runtimeOnly  compileOnly  ksp
testImplementation  androidTestImplementation  copy  exec  description
```

Contextual keywords keep the reserved set tiny, which keeps the grammar
open for future keys without breaking existing files, and lets
`tree-sitter-ulb` highlight them via a contextual-keyword rule rather than
hard-coded tokens.

---

## 5. Syntax (EBNF)

```
file            = { statement } EOF ;

statement       = if_statement
                | block_statement
                | def_statement
                | apply_statement
                | assignment
                | pair_statement
                | call_statement ;

(* --- control flow --- *)

if_statement    = "if" expression block [ "else" ( block | if_statement ) ] ;

(* --- blocks --- *)

block_statement = path block ;
path            = IDENT { "." IDENT } ;
block           = "{" { statement } "}" ;

(* --- definitions (conventions.ulb / build.ulb) --- *)

def_statement   = convention_def | fn_def | task_def ;
convention_def  = "convention" IDENT block ;
fn_def          = "fn" IDENT "(" [ IDENT { "," IDENT } ] ")" block ;
task_def        = "task" STRING block ;

(* --- application --- *)

apply_statement = "apply" STRING ;

(* --- assignment ("=" form, used in libs.ulb and versions/bundle/plugins) --- *)

assignment      = path "=" expression ;

(* --- bare pair (compileSdk 37, lspCompat true, implementation "g:a:v") --- *)

pair_statement  = IDENT expression ;

(* --- call (ver(...), env(...), maven(...), copy(...)) --- *)

call_statement  = call ;

(* --- expressions --- *)

expression      = or_expr [ "@" ( STRING | IDENT ) ] ;
or_expr         = and_expr { "||" and_expr } ;
and_expr        = not_expr { "&&" not_expr } ;
not_expr        = "!" not_expr | comparison ;
comparison      = primary [ compare_op primary ] ;
compare_op      = "==" | "!=" | "<" | "<=" | ">" | ">=" ;

primary         = STRING
                | NUMBER
                | BOOL
                | path
                | call
                | member_access
                | list
                | "(" expression ")" ;

call            = IDENT "(" [ argument { "," argument } ] ")" ;
argument        = expression | IDENT "=" expression ;
member_access   = call { "." IDENT } ;
list            = "[" [ expression { "," expression } ] "]" ;
```

### 5.1 Statement disambiguation (how the parser dispatches)

All statement forms except `if`/`def`/`apply` begin with `IDENT`, so the
parser reads a `path` first, then dispatches on the next token:

| Next token after `path` | Statement form |
|---|---|
| `{` | `block_statement` |
| `=` | `assignment` |
| `(` and path is a single `IDENT` | `call_statement` |
| anything else | `pair_statement` (a value expression is then required) |

Notes:

- A dotted `path` followed by `(` is not a call — a dotted callee is
  invalid and must be reported as a parse error.
- If a `path` is followed by nothing usable (end of file, or the next
  token begins a new statement), the parser reports *missing value after
  `path`* and recovers (§11). This is what makes newline-insensitivity
  safe: `versionName` on its own line produces a diagnostic instead of a
  silent mis-parse.

### 5.2 Expression notes

- **Precedence** (loosest to tightest): `@` (version attach) →
  `||` → `&&` → `!` → comparison → primary.
- `@` attaches a version to a coordinate string or alias reference:
  `alias = "group:artifact" @ coreVersion`. It is valid only at the top
  level of an assignment or pair value; the evaluator rejects it anywhere
  else.
- `argument = IDENT "=" expression` is a *named* argument (one-token
  lookahead: `IDENT` followed by `=`); otherwise the argument is
  positional. A call may mix neither — either all named or all positional
  (evaluator-enforced).
- `member_access` is restricted to builtin call results (`props(...)`).
  There is no general object-property chain; `props("path").key` is the
  only legal member access.

---

## 6. Statements by file role

### 6.1 `settings.ulb`

One per project. Declares the project name, module list, extra
repositories, and cache-compat flag.

| Statement | Meaning |
|---|---|
| `project "Name"` | project display/group name (exactly one; duplicate is an error) |
| `module "app"` | module path relative to project root (repeatable) |
| `repositories { maven "https://repo.example/m2" }` | extra Maven repos, additive over Google Maven + Maven Central, tried in declared order |
| `lspCompat true` | opt-in Gradle-cache artifact hardlinks for external LSPs (default `false`) |

Example:

```
project "SampleApp"
module "app"
module "shared"

repositories {
  maven "https://maven.pkg.jetbrains.space/public/p/compose/dev"
}

lspCompat true
```

### 6.2 `build.ulb`

One per module (the `module` entries in `settings.ulb` locate them).

| Statement | Meaning |
|---|---|
| `plugin "alias"` | apply a plugin by alias from `libs.ulb` `plugins {}` |
| `apply "name"` | apply a `convention` from `conventions.ulb` |
| `android { ... }` | Android configuration — keys owned by the `ulite/android` plugin (Appendix A) |
| `buildTypes { ... }` | build types — plugin-owned (Appendix A) |
| `productFlavors { ... }` | product flavors and dimensions — plugin-owned (Appendix A) |
| `signing { ... }` | signing config — plugin-owned (Appendix A) |
| `deps { ... }` | module dependencies |
| `<sourceSet>.deps { ... }` | source-set-scoped dependencies (§6.4) |
| `task "name" { ... }` | custom task (§6.5) |
| `if ... { } else { }` | top-level conditional configuration |
| `env("VAR")`, `props("path")`, `ver(...)` | native builtins as values |
| `description "..."` | module description (informational) |

Example:

```
plugin "android"

apply "android-app"
apply "env-signing"

android {
  namespace "com.example.app"
  compileSdk 37
  minSdk 24
  targetSdk 37
  applicationId "com.example.app"
  versionCode 7
  versionName ver(major=0, minor=1, patch=2)
}

buildTypes {
  debug { minifyEnabled false }
  release {
    minifyEnabled true
    proguardFiles [ "proguard-rules.pro" ]
  }
}

productFlavors {
  dimension "tier"
  free  { applicationIdSuffix ".free" }
  paid  { applicationIdSuffix ".paid" }
}

signing {
  storeFile   props("signing.properties").storeFile
  keyAlias    props("signing.properties").keyAlias
  storePassword env("STORE_PASSWORD")
  keyPassword   env("KEY_PASSWORD")
}

deps {
  implementation "androidx.core:core-ktx" @ coreVersion
  implementation appcompat
}

commonMain.deps {
  implementation kotlinxCoroutines
}
androidMain.deps {
  implementation "org.jetbrains.compose.ui:ui" @ composeVersion
}

task "printConfig" {
  description "Prints the resolved module configuration."
  dependsOn [ "compileReleaseKotlin", "bundleRelease" ]
  run {
    exec(command="echo", args=["hello", "from", "ulb"])
    copy(from="src/main/kotlin", to="out/merged-kotlin")
  }
}
```

### 6.3 `conventions.ulb`

Reusable configuration blocks and helper functions, globally visible to
every `build.ulb`. No import statement; everything here is in scope.

| Statement | Meaning |
|---|---|
| `convention NAME { ... }` | named reusable config block, applied via `apply "NAME"` |
| `fn name(p1, p2) { ... }` | pure helper function (no side effects except builtins it calls) |

Conventions may contain any statements a `build.ulb` may (including
`if`/`else`, `env`, `props`, nested `convention`-internal blocks), plus
`apply` of other conventions. `fn` bodies may contain any statements
except `convention`, `fn`, and `task` definitions.

Example:

```
convention android-app {
  android {
    compileSdk 37
    minSdk 24
    targetSdk 37
  }
  buildTypes {
    debug { minifyEnabled false }
    release { minifyEnabled true }
  }
}

convention env-signing {
  signing {
    storeFile   props("signing.properties").storeFile
    storePassword env("STORE_PASSWORD")
  }
}

fn defaultDebug() {
  buildTypes { debug { minifyEnabled false } }
}
```

*(Note: arithmetic and string concatenation are **not** part of the ulb
grammar (§7). `fn` helpers compose existing statements and values; they
receive and return values via named arguments and the `ver()` builtin
(Appendix C).)*

### 6.4 `libs.ulb`

Version catalog — the equivalent of `libs.versions.toml`'s `[versions]`,
`[libraries]`, `[bundles]`, and `[plugins]` tables.

| Statement | Meaning |
|---|---|
| `versions { NAME = "1.2.3" }` | named version strings |
| `alias = "group:artifact"` | coordinate without version (requires `@`) |
| `alias = "group:artifact:version"` | full coordinate |
| `alias = "group:artifact" @ refOrString` | coordinate with version reference |
| `bundle { NAME = [ alias1, alias2 ] }` | named group of library aliases |
| `plugins { NAME = "vendor/name" @ refOrString }` | versioned plugin references (Ulite Team plugin registry — ARCHITECTURE.md §3.3/§3.6) |

Example:

```
versions {
  coreVersion = "1.15.0"
  composeVersion = "1.8.0"
}

appcompat = "androidx.appcompat:appcompat:1.7.0"
coreKtx   = "androidx.core:core-ktx" @ coreVersion
ui        = "org.jetbrains.compose.ui:ui" @ composeVersion
kotlinxCoroutines = "org.jetbrains.kotlinx:kotlinx-coroutines-core" @ "1.9.0"

bundle {
  ui = [ ui, appcompat ]
}

plugins {
  android = "ulite/android" @ "1.4.0"
  kotlinJvm = "ulite/kotlin-jvm" @ "2.1.0"
}
```

An alias may only reference `@` a `versions {}` entry or an inline string.
An alias whose value is a full coordinate may not also carry `@` (duplicate
version — evaluator error). Plugin names resolve against the Ulite Team
plugin registry, not a Maven repository (ARCHITECTURE.md §3.6); the
evaluator treats them as opaque values — the tool layer resolves them
(`uliab plugins resolve` reads the `plugins {}` table and downloads each
referenced build into the local plugin cache, ARCHITECTURE.md §9,
steps 5–6).

---

## 7. Explicitly not in the language

The following are **syntax errors** by design; their absence is what keeps
the grammar small enough for a day-one LSP:

- Classes, interfaces, objects, inheritance, generics, lambdas, closures.
- Arithmetic operators (`+`, `-`, `*`, `/`, `%`) and unary minus.
  (Version arithmetic is handled by the `ver()` builtin; there is no need
  for general math in a build config.)
- Assignments to existing variables (mutation); `=` only binds a catalog
  alias/version/plugin name.
- Import/include statements (everything in `conventions.ulb`/`libs.ulb` is
  globally visible).
- File I/O from user code (`props()` and `env()` are the only builtins
  that touch the outside world).
- Statement terminators (`;`), significant indentation, string
  concatenation.

---

## 8. Grammar–tree-sitter portability notes

`tree-sitter-ulb` mirrors this grammar one-to-one. Mapping rules:

1. **Terminals** (`IDENT`, `NUMBER`, `STRING`, `BOOL`, comments) become
   named/external tokens with the same lexical definitions.
2. **Reserved words** become keyword tokens; **contextual identifiers**
   stay `identifier` tokens and get their highlight/role via context
   (matching this grammar, which never reserves them).
3. **Disambiguation** in §5.1 is expressed as an `alternatives` rule in
   `grammar.js` (tree-sitter resolves the `{`/`=`/`(`/value choice
   deterministically since each branch starts with a distinct token).
4. **Interpolation** is modeled as a `string` node containing
   `interpolation` child nodes — standard tree-sitter string handling.
5. No rule requires unbounded lookahead: every choice is decided by the
   current token or a one-token lookahead, which is GLR-friendly.

The `grammar.js` is maintained by hand against this document and the
sync-by-hand risk is tracked in ARCHITECTURE.md §11.

---

## 9. Expression semantics (what values exist)

| Value | Produced by | Notes |
|---|---|---|
| string | `STRING` literal, `env(...)` result | interpolation applied at lex time |
| number | `NUMBER` literal | integer or decimal |
| boolean | `BOOL` literal, comparisons, `&&`/`||`/`!` | short-circuit `&&`/`||` |
| version | `ver(...)` call | struct with `major`/`minor`/`patch` |
| properties | `props("path")` | mapping; `.key` member access |
| list | `[...]` literal | used by `bundle`, `dependsOn`, `args`, `proguardFiles` |
| reference | `path` | resolves an alias/version/plugin/fn name |

Boolean truthiness: a `path` used as a condition is invalid (evaluator
error) except where a specific key is declared boolean. `if` conditions
must evaluate to boolean; type mismatches are span-attached errors (§11).

---

## 10. Role validation

The parser accepts any `.ulb` file. Which statements are legal in which
role is enforced outside the parser (the LSP parses every `.ulb` file
identically and surfaces violations as semantic diagnostics, not parse
errors):

- `convention`/`fn` definitions only in `conventions.ulb` — enforced by
  the **core evaluator** (span-attached error).
- `project`/`module`/`repositories`/`lspCompat` only in `settings.ulb` —
  enforced by the tool layer (build pipeline, ARCHITECTURE.md §9).
- `versions`/`bundle`/`plugins`/alias assignments only in `libs.ulb` —
  enforced by the tool layer.
- `task`/`plugin` statements only in `build.ulb` (or inside a `convention`
  in `conventions.ulb`) — enforced by the tool layer.
- `android`/`buildTypes`/`productFlavors`/`signing` blocks are **not**
  core-validated: to the grammar and the evaluator they are ordinary
  blocks (§5), and their placement and keys are owned by whichever plugin
  claims them — the `ulite/android` plugin (ARCHITECTURE.md §5.2, and the
  ownership note at the top of Appendix A).
- `apply "NAME"` with no matching `convention NAME` is an error — the
  evaluator reports it, and the LSP surfaces it as a semantic diagnostic.

---

## 11. Error recovery & diagnostics contract

The parser must **never** fail-fast on malformed input. It produces a
partial AST plus a list of diagnostics; the evaluator and LSP both consume
this form.

- **Recovery strategy:** panic-mode, token-level. On an unexpected token,
  emit a diagnostic, discard tokens until the next statement start (an
  `IDENT`, reserved word, or `}` at the current nesting depth), and resume.
  Unterminated strings/blocks recover at the enclosing block boundary.
- **Diagnostic shape:** every diagnostic carries a `file:line:col` span
  and a severity. Formatted text: `file:line:col: error: <message>`
  (or `warning:`/`info:`). The LSP maps spans directly to
  `Diagnostic`/`Range`.
- **Partial AST:** recovered nodes are marked `invalid` so consumers (LSP
  semantic analysis, evaluator) can skip or surface them without guessing.
- The parser is deterministic: identical input always yields identical
  AST + diagnostics.

Required diagnostic cases (each gets an assertion test in `ulb-lang`):

- unexpected token / unexpected end of block
- missing value after `path`
- dotted callee (`a.b(...)`)
- unterminated string / unterminated block / unterminated interpolation
- reserved word used as identifier
- `@` used where no version attach is allowed

---

## Appendix A — `android {}`, `buildTypes {}`, `productFlavors {}`, `signing {}`

> **Ownership note.** The tables in this appendix are *reference* material
> for the plugin that owns these blocks; they are not a claim that the
> core tool understands these keys. To the ulb grammar and evaluator,
> `android {}`, `buildTypes {}`, `productFlavors {}`, and `signing {}` are
> ordinary block statements (§5) whose contents resolve to a generic
> `Value` block — the same as any other block. The `ulite/android` plugin
> (ARCHITECTURE.md §5.2) claims these keys, validates and interprets them,
> and publishes its own reference docs mirroring these tables. The core
> parser and evaluator never interpret the keys inside them (§10).

### `android { ... }`

| Key | Type | Meaning |
|---|---|---|
| `namespace` | string | Kotlin/AGP namespace |
| `applicationId` | string | final app id |
| `compileSdk` | number | compile SDK level |
| `minSdk` | number | minimum SDK level |
| `targetSdk` | number | target SDK level |
| `versionCode` | number | integer version code |
| `versionName` | `ver(...)` or string | display version |
| `signing { ... }` | block | see below (also valid at top level of `build.ulb`) |

### `buildTypes { ... }`

Named blocks (`debug`, `release`, or custom). Keys:

| Key | Type | Meaning |
|---|---|---|
| `minifyEnabled` | boolean | R8/minification |
| `shrinkResources` | boolean | resource shrinking |
| `proguardFiles` | list of strings | proguard rule files |

### `productFlavors { ... }`

- `dimension "tier"` (pair statement, repeatable) declares a flavor
  dimension.
- Flavor blocks (`free { }`, `paid { }`) may carry `dimension "name"` and
  any of:

| Key | Type | Meaning |
|---|---|---|
| `dimension` | string | which dimension this flavor belongs to |
| `applicationIdSuffix` | string | appended to `applicationId` |
| `versionNameSuffix` | string | appended to `versionName` |
| `minSdk` | number | flavor-specific floor |

The variant matrix is the cartesian product of build types × flavors; a
variant is *valid* only if it contains exactly one flavor per dimension
(see ARCHITECTURE.md §4).

### `signing { ... }`

| Key | Type | Meaning |
|---|---|---|
| `storeFile` | string (`props(...).key` or literal) | keystore path |
| `storePassword` | string (`env(...)` or literal) | store password |
| `keyAlias` | string | key alias |
| `keyPassword` | string | key password |

---

## Appendix B — `deps {}` scopes

Inside `deps { }` (or `<sourceSet>.deps { }`), pair statements whose key is
a scope name:

| Scope | Visibility |
|---|---|
| `api` | transitive to consumers (visible on consumer compile classpath) |
| `implementation` | module-private (does not leak) |
| `runtimeOnly` | runtime only, not compile |
| `compileOnly` | compile only, not runtime |
| `ksp` | annotation-processor classpath (KSP step) |
| `testImplementation` | `test` compilation/runtime |
| `androidTestImplementation` | `androidTest` compilation/runtime |

Each takes a coordinate string, an alias reference, or a
`"group:artifact" @ ref` versioned coordinate.

---

## Appendix C — builtin functions

| Builtin | Form | Returns | Errors |
|---|---|---|---|
| `env` | `env("NAME")` | string | missing variable → error |
| `props` | `props("path")` / `props("path").key` | properties mapping / string | missing file or key → error |
| `ver` | `ver(major=N, minor=N, patch=N)` | version value | missing/wrong-typed arg → error |
| `maven` | `maven "https://..."` (pair) | repository declaration | — |
| `plugin` | `plugin "alias"` (pair) | plugin application | unknown alias → error |
| `copy` | `copy(from="...", to="...")` | task action | only inside `run {}` |
| `exec` | `exec(command="...", args=[...])` | task action | only inside `run {}`; command must be allowlisted |

`env`, `props`, `ver` are usable in any expression position. `copy`/`exec`
are usable only as statements inside a `task`'s `run {}` block.

---

## Appendix D — `task {}` body

```
task "name" {
  description "..."               (* optional *)
  dependsOn [ "task1", "task2" ]  (* optional; task names as strings *)
  run {
    (* action statements: copy(...), exec(...) — Appendix C *)
  }
}
```

- `dependsOn` names refer to tasks by the same `"name"` string. A
  `dependsOn` reference to an undefined task is a resolution error.
literal:- The `run` body is a closed action set — no arbitrary code.
literal:
