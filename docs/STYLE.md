# Rust style

This guide states how Rust is written here, one numbered rule per section with
an example. The lint set that `dev init` installs enforces the mechanical rules,
and each rule names its check on a `Lint:` line or says `Review` when a reviewer
checks it. A project adds its own rules under the `## This project` section that
`dev init` appends.

## At a glance

| Context | Mood | Case | Trailing period |
|---|---|---|---|
| `///` summary | third person ("Creates", "Returns") | Capitalized | yes |
| `///` on a const or field | noun phrase | Capitalized | yes |
| Inline `//` comment | present tense, "we" welcome | Capitalized | no |
| `// NOTE:` and `// TODO:` | marker, then a sentence | Capitalized | no |
| Error `Display` message | phrase | lowercase | no |
| Rustdoc section | `# Examples`, `# Errors`, `# Panics`, `# Safety` | h1 | |

## Comments

Make the code say it first, and comment only what the code cannot say.

### 1. Restructure instead of writing a long comment

Restructure the code when a `//` comment runs to three or more consecutive lines
of solid prose. Never split a paragraph into bullets to get under the limit. A
long doc comment is fine.

Lint: `lint.sh` comment runs, which passes a run that holds a numbered list item

```rust
// Good
let normalized = path.components().fold(PathBuf::new(), push_normalized);

// Bad
// Walk every component, skip `.` entirely, and pop the last pushed
// component on `..` unless only the root is left, in which case the `..`
// is dropped so the result never climbs above the root.
let mut normalized = PathBuf::new();
```

### 2. List separate cases as a numbered list

Enumerate cases inside a comment as a numbered list, one line per case, and only
when the cases are genuinely separate.

Review

```rust
// Load order matters for the script:
// 1. Fonts are registered first so the script can name them
// 2. Pages are created after it runs so they see its settings
let fonts = Fonts::load_from_dir(&config.font_dir)?;
```

### 3. Write a one-liner capitalized, present tense, no period

A `//` one-liner starts with a capital, reads in the present tense and has no
trailing period. "We" is welcome. `dev fmt` removes the period from a one-liner and
leaves a multi-line run, a doc comment and an ellipsis alone.

Lint: `lint.sh` comment periods

```rust
// Good
// If output is not specified, we use the title with a .pdf extension
let output = args.output.unwrap_or_else(|| title.with_extension("pdf"));

// Bad
// output defaults to the title.
let output = args.output.unwrap_or_else(|| title.with_extension("pdf"));
```

### 4. Flag a caveat with NOTE and pending work with TODO

Use `// NOTE:` for a caveat and `// TODO:` for pending work, and hang-indent
continuation lines under the text.

Review

```rust
// NOTE: We keep the trailing separator so joining a file name onto this path
//       never has to check whether one is already there.
let mut base = root.as_os_str().to_owned();
```

### 5. Put a blank line before a full-line comment

A full-line comment that follows code at the same or a deeper indent, such as the
last line of a method chain, gets a blank line above it. An end-of-line comment
needs none.

Lint: `lint.sh` comment spacing, with the exceptions in the style skill's `SKILL.md` Style rule 1

```rust
// Good
let config = load_config()?;

// Apply overrides from the environment
config.apply_env_overrides();
let timeout = 30; // seconds

// Bad
let config = load_config()?;
// Apply overrides from the environment
config.apply_env_overrides();
```

### 6. Head each block of a long function with one intent line

Give each block of a multi-block function one comment line saying what it is
for.

Review

```rust
pub fn run(config: &Config) -> anyhow::Result<()> {
    // Load the fonts the script may reference
    let fonts = Fonts::load_from_dir(&config.font_dir).context("Failed to load fonts")?;

    // Run the script against a fresh document
    let document = Script::load_from_path(&config.script)
        .and_then(|script| script.run(&fonts))
        .context("Failed to run script")?;

    // Write the finished document to disk
    document.save(&config.output).context("Failed to save document")
}
```

### 7. Write a comment as a principle in the present tense

Say what the code is and why, in the module's own vocabulary. Never narrate
history, speculate about the future, name the code that calls it, or point at a
file, a line or a task id. Every comment, `//`, `///` and `//!` alike, also
follows the `prose-review` skill.

A pointer to another place in the code rots the moment that code moves.

Review

```rust
// Good
// Separators are normalized first so both slash forms parse the same

// Bad
// Moved here from parser.rs, see TODO-142
```

### 8. Replace a warning comment with a type

When a comment asks callers to uphold an invariant, change the structure so the
invariant cannot be broken.

Review

```rust
// Good: each parse gets fresh scratch space
pub fn parse(input: &str) -> Result<Ast, ParseError> {
    Scratch::new().parse(input)
}

// Bad
pub struct Parser {
    // NOTE: Callers must clear this before every parse
    scratch: Vec<Token>,
}
```

## Rustdoc

Rustdoc follows std: third person, linked, and limited to what the caller can
observe.

### 9. Write the summary as one third-person sentence

Open with "Creates", "Returns", "Represents a" and the like, never a bare
imperative.

Rust docs follow std's third person even though Lua doc summaries are
imperative.

Review

```rust
// Good
/// Returns the final component of this path, if there is one.
pub fn file_name(&self) -> Option<&str> {
    self.components().next_back().and_then(Component::as_normal)
}

// Bad
/// Return the file name.
```

### 10. Add a paragraph only for what the caller can observe

After the summary, write only an edge case, a no-op condition, an equivalence to
another method, or what stays unchanged. Mechanism, history and a restatement of
the signature never appear.

Review

```rust
/// Updates [`self.extension`] to `extension`.
///
/// Returns `false` and does nothing if [`self.file_name`] is [`None`],
/// returns `true` and updates the extension otherwise.
pub fn set_extension(&mut self, extension: &str) -> bool {
```

### 11. Pick the doc register by crate type

A library documents every public item, gives nearly every `pub fn` an
`# Examples` section, and doctests its README. An application gives every item,
every field included, a one-line `///` summary and has no doctests.

Test-support code (`tests/common/*`, helpers inside an integration-test crate,
`#[cfg(test)]` modules) takes `//` summaries where it needs one. The `///` above
applies to code the crate ships.

Lint: `clippy::missing_errors_doc`, `missing_panics_doc` in a library,
`missing_safety_doc` everywhere. `# Examples` is Review.

```rust
#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;
```

### 12. Link items and backtick parameters

Name every item in a doc with an intra-doc link, and backtick a parameter or a
literal value. A parameter cannot be linked, which is the whole split.

Review

```rust
/// Joins `path` onto this [`PathBuf`], returning [`CheckedPathError`] if `path`
/// would escape it.
pub fn join_checked(&self, path: impl AsRef<Path>) -> Result<PathBuf, CheckedPathError> {
```

### 13. Head rustdoc sections at h1

Use `# Examples`, `# Errors`, `# Panics` and `# Safety`, never a smaller
heading.

Lint: `clippy::missing_errors_doc`, `missing_panics_doc`, `missing_safety_doc`
for presence, and `lint.sh` doc headings for the level.

```rust
// Good
/// Parses `input` as a terminal size in `ROWSxCOLS` form.
///
/// # Errors
///
/// Returns [`SizeParseError::MissingRows`] if `input` has no rows.

// Bad
/// ### Errors
```

### 14. Document a module as an essay or one line

A public module that is a real API surface gets a `//!` essay with at least one
`#` section, as `std::io` has. Any other `pub mod` gets one `///` line at its
declaration. A private module and a `#[cfg(test)]` module get nothing.

Lint: `lint.sh` module docs, which flags a `//!` block under four lines

```rust
/// Contains constants associated with different path formats.
pub mod constants;

mod parser;
```

## Naming

A name says what a thing is for, and the folder or crate carries the context.

### 15. Name a file with one lowercase word

Let the folder carry the context. A second word needs a real ambiguity in the
same folder.

Review

```text
Good: src/pdf/object/circle.rs
Bad:  src/pdf/object/pdf_circle.rs
```

### 16. Name a type the way its crate already does

In a multi-crate workspace, use plain names inside the crate (`Client`,
`Server`). In a single crate, prefix the type with its module path in
PascalCase.

In a single crate every type is re-exported at the root, so the prefix is the
namespace and `clippy::module_name_repetitions` stays off.

A type that never reaches the crate root (`pub(super)`, private, or
`pub(crate)` used only inside its module) is named for its role in its own
file, with no module prefix: `wire.rs` holds `WireTask` and `WireComment`
(agentctl `src/board/chipflow/wire.rs:115`). The prefix keeps a name
unambiguous at the root, and a type that stays in its file never meets another
there. Once a type is re-exported at the root, the module-path prefix applies
and it takes the full path (`BoardChipflow`).

Review

```rust
// Single crate, src/pdf/object/circle.rs
pub struct PdfObjectCircle {
    pub center: PdfPoint,
    pub radius: f32,
}
```

### 17. Flatten a generic type with an alias

Give each common instantiation of a generic type its own name.

Review

```rust
pub type UnixPath = Path<UnixEncoding>;
pub type ParseResult<'a, T> = Result<(&'a [u8], T), ParseError>;
```

### 18. Name a conversion by whether it borrows or consumes

`as_*` borrows, `to_*` converts, `into_*` consumes, `with_*` makes a variant,
and `*_checked` is the fallible twin. Sourced constructors read `load_from_*`
and `add_from_*`, and an accessor takes the bare noun with no `get_`.

Lint: `clippy::wrong_self_convention` for `as_`, `to_`, `into_` and `is_`.
`lint.sh` getters for `get_`.

```rust
// Good
fn as_path(&self) -> &Path;
fn to_string_lossy(&self) -> Cow<'_, str>;
fn into_string(self) -> String;
fn with_extension(&self, extension: &str) -> PathBuf;
fn push_checked(&mut self, path: impl AsRef<Path>) -> Result<(), CheckedPathError>;
fn file_name(&self) -> Option<&str>;

// Bad
fn get_file_name(&self) -> Option<&str>;
```

### 19. Name a thing for its purpose in as few words as tell it apart

A Java-length name usually means two concerns in one thing.

Review

```rust
// Good
pub struct Retry {
    pub attempts: u32,
}

// Bad
pub struct ConnectionRetryAttemptCounterState {
    pub attempts: u32,
}
```

### 20. Give a closed set of values an enum

Use an enum or named constants, never raw strings spread through the code. Ids,
paths and user-supplied keys are open-ended and not a set.

Review

```rust
// Good
pub enum Separator {
    Slash,
    Backslash,
}

let parts = split(path, Separator::Slash);

// Bad
let parts = split(path, "slash");
```

### 21. Name a limit once, with its unit

Name a threshold, timeout or limit as one constant carrying its unit, never as a
multiplier over a base. A genuine ratio stays a ratio.

Review

```rust
// Good
const READ_TIMEOUT_SECS: u64 = 30;

// Bad
const READ_TIMEOUT_SECS: u64 = BASE_TIMEOUT_SECS * TIMEOUT_SCALE;
```

## Signatures and visibility

The caller writes as little as possible, and sees as little as it needs.

### 22. Bundle related options in a config struct

Pass one struct instead of a run of loosely related arguments.

Review

```rust
// Good
pub fn connect(addr: SocketAddr, config: ClientConfig) -> io::Result<Client> {

// Bad
pub fn connect(
    addr: SocketAddr,
    retries: u32,
    timeout: Duration,
    nodelay: bool,
) -> io::Result<Client> {
```

### 23. Take `impl Into`, `impl AsRef` or `impl IntoIterator`

Accept the generic form so the caller never writes `Some(...)` or a clone at
the call site.

Review

```rust
pub fn connect_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
    self.connect_timeout = timeout.into();
    self
}

// Good
builder.connect_timeout(Duration::from_secs(5));

// Bad, with an `Option<Duration>` parameter
builder.connect_timeout(Some(Duration::from_secs(5)));
```

### 24. Pass a mode as an enum, never as booleans

A domain enum names the mode at the call site where a `true` says nothing.

Lint: `clippy::fn_params_excessive_bools` at two or more parameters,
`struct_excessive_bools` at four or more fields

```rust
// Good
canvas.stroke(&shape, LineCap::Round, Winding::NonZero);

// Bad
canvas.stroke(&shape, true, false);
```

### 25. Keep items private by default

Reach for `pub(crate)` sparingly. Expose plain-data fields as `pub` and keep
invariant-bearing fields private on the same type.

Review

```rust
pub struct Page {
    pub id: PageId,
    pub title: String,
    objects: Arc<RwLock<Vec<Object>>>,
}
```

### 26. Close a trait set with a sealed supertrait

A private supertrait keeps other crates from implementing the trait.

Review

```rust
mod private {
    pub trait Sealed {}
}

pub trait Encoding: private::Sealed {
    fn label() -> &'static str;
}
```

### 27. Use a leading underscore only to discard a value

An underscore marks a value nothing reads, never a private name.

Review

```rust
pub struct Path<T> {
    _encoding: PhantomData<T>,
    inner: [u8],
}
```

### 28. Never reach into another type's private state

Readers get a snapshot through the type that holds the state, and writers go
through its methods, in production and in tests.

Review

```rust
// Good
let stats = cache.stats();
assert_eq!(stats.hits, 1);

// Bad
assert_eq!(cache.entries.len(), 1);
```

### 29. Put nothing in a production module that exists only for tests

The test drives the real path, or the seam is injected and production uses it
too.

Review

```rust
// Good
client.send(Request::Shutdown).await?;

// Bad
server.force_shutdown_for_tests = true;
```

### 30. Keep one mechanism per concept

Two surfaces for one feature share one shape with a discriminator, never a
simple form here and a full form there.

Review

```rust
// Good
pub struct Length {
    pub value: f32,
    pub unit: Unit,
}

// Bad
pub struct Layout {
    pub width_px: u32,
    pub width: Option<Length>,
}
```

## Modules

A crate keeps its modules private and its paths at the top of the file.

### 31. Import a type with `use` at the top of the file

Never write `super::` or `crate::` inline in a signature, a field or an
expression.

Inline paths hide a signature's real dependencies.

Review

```rust
// Good
use crate::config::PasswordFeedback;

fn prompt(feedback: PasswordFeedback) -> Result<String> {

// Bad
fn prompt(feedback: super::PasswordFeedback) -> Result<String> {
```

### 96. Put a `#[cfg]`-gated import inside the item it serves

A `#[cfg(...)]`-gated import goes in the gated `mod` or the gated function's
body, never at the top level of a file. rustfmt never moves an import between
scopes, so the attribute stays on the right import. A gated `pub use` or
`pub(crate) use` re-export stays at the top level.

Lint: `lint.sh` cfg imports

```rust
// Good
use std::io;
use std::process::Command;

#[cfg(windows)]
fn detach(command: &mut Command) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    command.creation_flags(DETACHED_PROCESS);
    Ok(())
}

// Bad
use std::io;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;
```

### 32. Declare a module as a named file plus a directory

Keep `mod.rs` only under `tests/`, as a shared helper for integration-test
binaries.

Lint: `clippy::mod_module_files`

```text
Good: src/parser.rs, src/parser/lexer.rs
Bad:  src/parser/mod.rs, src/parser/lexer.rs
```

### 33. Order a module file as mods, re-exports, then items

The `mod` list comes first, its `pub use` re-exports next, the file's own items
last.

Review

```rust
mod lexer;
mod token;

pub use lexer::Lexer;
pub use token::{Token, TokenKind};

pub struct Parser {
    lexer: Lexer,
}
```

### 34. Re-export from the root, and make a module public only for its namespace

Keep modules private and re-export their items from the crate root. Make a
module public only for a group of free functions and constants, for same-named
constants told apart by module, or for a facade crate's sub-crates.

Review

```rust
mod common;
mod unix;
mod windows;

pub mod utils; // read as utils::current_dir

pub use common::*;
pub use unix::*;
pub use windows::*;
```

### 35. Re-export a facade's sub-crates as short-named modules

One line per sub-crate, and nothing hoisted to the facade's root.

Review

```rust
pub use foo_core as core;
pub use foo_net as net;
```

### 36. Ship a prelude only when callers would import a handful of items

Fill `pub mod prelude` with types and traits only, under their own names. Free
functions and constants stay reachable through their module.

Review

```rust
pub mod prelude {
    pub use crate::{Client, ClientConfig, Request, Response, TransportExt};
}
```

### 37. Glob-import only preludes, macro packs and the domain module

Every other import names its items.

Review (`clippy::wildcard_imports` stays off)

```rust
use log::*;
use mlua::prelude::*;

use crate::pdf::*;
```

### 38. Group imports std, external, then the crate

Import a function through its module and a type directly.

Lint: rustfmt `group_imports`, `imports_granularity`

```rust
use std::io;
use std::path::PathBuf;

use anyhow::Context;

use crate::config::Config;
use crate::fs;

let config = Config::load_from_path(&path)?;
let bytes = fs::read_all(&path)?;
```

## Constants

A constant lives with whatever owns it.

### 39. Keep a constant beside what uses it

A constant used by one file stays in that file. In a binary, a constant several
files share goes in a top-level `constants.rs`. In a library, it goes in the
module that owns the concept and is re-exported from the root.

A constant that several modules need and none owns is shared logic that wants a
module of its own.

Review

```rust
// Library, src/codec.rs, read as codec::MAX_FRAME_SIZE
pub const MAX_FRAME_SIZE: usize = 16 * 1024;
```

### 40. Declare a type's own value as an associated const

A value that describes one type goes in that type's `impl`, as std moved
`u32::MAX` onto `u32`.

Review

```rust
impl Frame {
    /// Length of the frame header in bytes.
    pub const HEADER_LEN: usize = 8;
}
```

### 41. Doc-comment a binary's constants and make lazy values `LazyLock`

Fill `constants.rs` with doc-commented `pub const` and `static` items, grouped
into nested modules when they cluster.

Lint: `clippy::non_std_lazy_statics`, and `lint.sh` manifests for a direct
`once_cell` or `lazy_static` dependency

```rust
/// Maximum number of bytes read from a pipe per call.
pub const MAX_PIPE_CHUNK_SIZE: usize = 16384;

/// User-oriented paths.
pub mod user {
    /// Path to the configuration file.
    pub static CONFIG_FILE_PATH: LazyLock<PathBuf> =
        LazyLock::new(|| config_dir().join("config.toml"));
}
```

## Borrowing

Borrow by default, and own only what must outlive the call.

### 42. Pair a borrowed type with an owned buffer, and return `Cow`

Follow std's `&Path` and `PathBuf` pairing, and return `Cow` when a result is
maybe-owned.

Review

```rust
pub fn to_string_lossy(&self) -> Cow<'_, str> {
    String::from_utf8_lossy(&self.inner)
}
```

### 43. Hand a long-lived closure a `Weak`

Downgrade before the closure captures the state and upgrade per call, so the
closure cannot keep the state alive.

Review

```rust
let pages = Arc::downgrade(&self.pages);
let count = move || pages.upgrade().map_or(0, |pages| pages.read().len());
```

### 44. Implement `Deref` only for a pointer-shaped wrapper

A wrapper that adds policy exposes an explicit accessor instead.

`Deref` to the inner value lets a caller bypass the policy.

Review

```rust
// Good
impl Sandbox {
    pub fn lua(&self) -> &Lua {
        &self.lua
    }
}

// Bad
impl Deref for Sandbox {
    type Target = Lua;

    fn deref(&self) -> &Lua {
        &self.lua
    }
}
```

## Errors

A library hands callers an error they can match, and a binary tells the user
what failed.

### 45. Give a library its own error type, and a binary `anyhow`

A library returns a std error type when it is the only failure, or an enum with
`Display` and `Error` written by hand. Deriving them with `derive_more` is
allowed, and `anyhow` and `thiserror` never appear in library code.

Lint: `lint.sh` thiserror for a direct `thiserror` dependency. `anyhow` is Review.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    UnexpectedEnd,
    InvalidByte(u8),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of input"),
            Self::InvalidByte(byte) => write!(f, "invalid byte {byte:#04x}"),
        }
    }
}

impl std::error::Error for ParseError {}
```

### 46. Return `io::Error` only while every failure has a specific kind

The first `ErrorKind::Other` in library code means it is time for the crate's
own enum with an `Io(io::Error)` variant.

A caller matching on `kind()` learns nothing from `Other`.

Lint: `clippy::io_other_error`, `clippy::disallowed_methods` on
`io::Error::other`

```rust
// Good
pub enum LoadError {
    Io(io::Error),
    MissingHeader,
}

// Bad
return Err(io::Error::other("missing header"));
```

### 47. Write an error message as a lowercase phrase with no period

Give a derived `Display` a `#[display("...")]` string on every variant, or the
variant prints as its bare name.

Review

```rust
#[derive(Clone, Debug, PartialEq, Eq, Display, Error)]
pub enum SizeParseError {
    #[display("missing rows")]
    MissingRows,
    #[display("invalid rows: {_0}")]
    InvalidRows(ParseIntError),
}
```

### 48. Add context to every propagation in a binary

Put `.context()` or `.with_context()` on every `?`, and phrase it "Failed to
X".

Review

```rust
let config = Config::load_from_path(&path)
    .with_context(|| format!("Failed to load config from {}", path.display()))?;
```

### 49. Keep `unwrap()` for tests and doctests

Production paths return `Result`. A production `unwrap()` that must stay
carries an `#[expect]` with its reason.

Lint: `clippy::unwrap_used`, `clippy::allow_attributes_without_reason`

```rust
#[expect(clippy::unwrap_used, reason = "the pattern is a literal checked by tests")]
let pattern = Regex::new(r"^\d+x\d+$").unwrap();
```

## Traits and generics

A trait earns its place with a second implementation, and bounds stay readable.

### 50. Name an extension trait `*Ext`

A trait that adds methods to a foreign type ends in `Ext`.

Review

```rust
pub trait LuaTableExt {
    fn raw_get_ext<V: FromLua>(&self, key: &str) -> LuaResult<V>;
}

impl LuaTableExt for LuaTable {
```

### 51. Model a state machine as a typestate, and a builder the same way

Each transition is a consuming method. A builder's setters take the field's
bare name and swap a type parameter when the field's type changes.

Review

```rust
impl<H, C> ClientBuilder<H, C> {
    pub fn auth_handler<U>(self, auth_handler: U) -> ClientBuilder<U, C> {
    pub fn config(self, config: ClientConfig) -> Self {
}
```

### 52. Abstract a backend only when a second one exists

Put a backend behind one small trait only when a second implementation exists
or the trait is a plugin boundary several providers will fill.

A trait with one implementation is indirection that only makes the code harder
to follow.

Review

```rust
pub trait Backend {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
}

impl Backend for LocalBackend {
impl Backend for SshBackend {
```

### 53. Put impl-header bounds in a `where` block

Use a `where` block even for one bound. Const generics stay inline.

Lint: ast-grep `impl-inline-bound`

```rust
// Good
impl<T> Utf8Path<T>
where
    T: Utf8Encoding,
{

impl<'a, const N: usize> TryFrom<&'a [u8; N]> for UnixComponent<'a> {

// Bad
impl<T: Utf8Encoding> Utf8Path<T> {
```

### 54. Inline a short fn-level bound

A function bound that fits on one line stays inline.

Review

```rust
pub fn new<S: AsRef<[u8]> + ?Sized>(s: &S) -> &Self {
```

### 55. Elide lifetimes wherever the compiler allows

Write `'_` for an opaque borrow in an impl header, and name a lifetime that is a
domain concept after the domain (`'lua`). Otherwise use `'a`.

Lint: `clippy::needless_lifetimes`, `elidable_lifetime_names`, rustc
`elided_lifetimes_in_paths`

```rust
// Good
impl fmt::Display for WindowsComponent<'_> {

// Bad
impl<'a> fmt::Display for WindowsComponent<'a> {
```

## Attributes

Attributes appear in one order and only where they change behavior.

### 56. Write derives in one order

`Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash`, then
external derives such as serde and `derive_more`.

Lint: `lint.sh` derive order

```rust
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PageId(u32);
```

### 57. Give every public type `Debug`

Derive it or write it by hand.

Lint: rustc `missing_debug_implementations`

```rust
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub retries: u32,
}
```

### 58. Mark tiny hot methods `#[inline]` and borrowed wrappers `#[repr(transparent)]`

A wrapper over a borrowed slice is `repr(transparent)` so the cast to it is
sound.

Review

```rust
#[repr(transparent)]
pub struct Path<T> {
    _encoding: PhantomData<T>,
    inner: [u8],
}

impl<T> Path<T> {
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }
}
```

### 59. Declare wire shape at the type

Use serde's `rename_all` on the type, never a rename on each field.

Review

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub file_name: String,
    pub modified_at: u64,
}
```

### 60. Put `#[must_use]` where ignoring the result is a bug

Mark a method that reads like in-place mutation (`with_*`, `to_*`, builder
setters), a pure function whose result is the only reason to call it, and a
type whose silent drop loses something. Never mark a unit function or one whose
return type is already `must_use`.

Lint: `clippy::return_self_not_must_use`, `double_must_use`, `must_use_unit`.
The rest is Review.

```rust
#[must_use]
pub fn with_extension(&self, extension: &str) -> PathBuf {
```

## Unsafe and async

Unsafe code lives in one place and says why it is sound.

### 61. Explain every `unsafe` with why it is sound

An `unsafe fn` says it in a `# Safety` section, and an `unsafe` block in a
`// SAFETY:` comment directly above it. Keep it short and plain.

Lint: `clippy::undocumented_unsafe_blocks`, `clippy::missing_safety_doc`

```rust
// SAFETY: Path<T> is repr(transparent) over [u8], so the layouts match
unsafe { &*(bytes as *const [u8] as *const Self) }
```

### 62. Concentrate `unsafe` in one core and deny it elsewhere

The workspace lints deny `unsafe_code`. The core lifts it with a local
`#[expect]` only when the root `Cargo.toml` lists the crate's path under
`[workspace.metadata.style] allow-unsafe`, and only the owner adds an entry.

Lint: rustc `unsafe_code` at `deny`, `lint.sh` unsafe for an `allow` or `expect`
of `unsafe_code` in an unlisted crate, `clippy::multiple_unsafe_ops_per_block`

```toml
[workspace.metadata.style]
allow-unsafe = ["foo-path"]
```

```rust
#[expect(unsafe_code, reason = "Path<T> is a transparent cast over [u8]")]
pub fn new<S: AsRef<[u8]> + ?Sized>(s: &S) -> &Self {
```

### 63. Keep the async runtime in binaries

Pure-logic crates carry no runtime. A binary takes `tokio` with the features it
needs.

Review

```toml
# Binary crate
[dependencies]
tokio = { version = "1", features = ["full"] }
```

### 64. Write async trait methods as native `async fn`

Never use the `async-trait` crate.

Lint: `lint.sh` manifests, for a direct `async-trait` dependency

```rust
pub trait Transport {
    async fn send(&mut self, frame: Frame) -> io::Result<()>;
}
```

## Tests

A test name reads as a sentence, and a test runs against the real thing.

### 65. Put unit tests at the bottom of the file

Use `#[cfg(test)] mod tests` as the file's last item.

Lint: `clippy::items_after_test_module`

```rust
#[cfg(test)]
mod tests {
    use super::*;
}
```

### 66. Name a test in the `should` form, never `test_`

Write `<subject>_should_<behavior>` in the top-level `tests` module and
`should_<behavior>` in a per-function submodule, at whatever length the behavior
needs.

Lint: ast-grep `test-fn-prefix` for the `test_` prefix. The `should` form is
Review.

```rust
// Good
#[test]
fn join_checked_should_fail_if_path_escapes_root() {

mod normalize {
    #[test]
    fn should_remove_current_dir_components() {
}

// Bad
#[test]
fn test_join_checked() {
```

### 67. Run integration tests against real infrastructure

Put integration tests per crate under `tests/` and drive real servers, files and
processes, never mocks.

Review

```rust
#[test]
fn copy_should_preserve_file_contents() {
    let temp = assert_fs::TempDir::new().unwrap();
    temp.child("a.txt").write_str("hello").unwrap();

    Command::cargo_bin("copy")
        .unwrap()
        .args(["a.txt", "b.txt"])
        .current_dir(&temp)
        .assert()
        .success();
    temp.child("b.txt").assert("hello");
}
```

### 68. Compile a test everywhere unless the code under test is platform-specific

Branch inside a test of cross-platform code on `cfg!(windows)`, and keep
`#[cfg(windows)]` for code that compiles only there.

Review

```rust
// Good
#[test]
fn display_should_use_platform_separator() {
    let expected = if cfg!(windows) { r"a\b" } else { "a/b" };
    assert_eq!(join("a", "b").to_string(), expected);
}

// Bad
#[cfg(unix)]
#[test]
fn display_should_use_platform_separator() {
```

### 69. Treat a checked-in `#[ignore]` as a code smell

Zero is the goal, platform ignores included. One that must stay carries a
comment explaining it.

Lint: ast-grep `test-ignore`, which warns on every site

```rust
// Bad
#[cfg_attr(windows, ignore)]
#[test]
fn rename_should_replace_existing_file() {
```

### 70. Test with rstest, assert_fs and assert_cmd under nextest

Run `cargo nextest run` and then `cargo test --doc`, since nextest skips
doctests.

Lint: `test.sh`

```rust
#[rstest]
#[case("a/./b", "a/b")]
#[case("a/../b", "b")]
fn normalize_should_resolve_dot_components(#[case] input: &str, #[case] expected: &str) {
    assert_eq!(UnixPath::new(input).normalize(), UnixPathBuf::from(expected));
}
```

### 71. Investigate every failure

A failure is never dismissed as intermittent or as already there.

Review

```text
Good: rerun with the failing seed, find the race, fix it
Bad:  rerun until it passes
```

### 72. Add a property test only for a round-trip, a parser that never panics, or determinism

Hand-written cases come first and stay. Name the property test
`should_round_trip_...`, `should_not_panic_on_...` or
`should_be_deterministic_...`, and commit its `proptest-regressions/`
directory.

Review

```rust
proptest! {
    #[test]
    fn should_not_panic_on_arbitrary_bytes(bytes in any::<Vec<u8>>()) {
        let _ = parse(&bytes);
    }
}
```

## Manifests

Every member of a workspace builds from the same settings.

### 73. Lay out a workspace as a root binary plus library members

Name the members for their purpose, and inherit package, dependency and lint
settings from the workspace.

Review

```toml
[workspace]
members = ["foo-core", "foo-net"]

[lints]
workspace = true
```

### 74. Exact-pin internal dependencies

A caret range lets the members drift apart.

Lint: `lint.sh` manifests

```toml
# Good
foo-core = { version = "=0.20.0", path = "foo-core" }

# Bad
foo-core = { version = "0.20.0", path = "foo-core" }
```

### 75. Ship release builds small or fast by crate shape

Use `lto = true`, `codegen-units = 1` and `strip = true`, with `opt-level = "z"`
for a CLI and `opt-level = 3` for anything with a frame or request loop.

Review

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
opt-level = "z"
```

### 76. List dependencies alphabetically and trim unused features

Set `default-features = false` where the defaults go unused.

Lint: `cargo sort --check` for order. Feature trimming is Review.

```toml
[dependencies]
anyhow = "1"
derive_more = { version = "1", default-features = false, features = ["display", "error", "from"] }
log = "0.4"
```

### 77. Point at a fork through `[patch.crates-io]`

Fork a blocked upstream and patch it in, so the rest of the manifest keeps the
registry name.

Review

```toml
[patch.crates-io]
printpdf = { git = "https://github.com/example/printpdf.git", branch = "fix-fonts" }
```

### 78. Put `std` behind a default feature in a library that can be `no_std`

A library that needs `std` declares no such feature.

Review

```toml
[features]
default = ["std"]
std = []
```

### 79. Let an application's features select backends

Forward each feature to its member crate, and enable an optional dependency as
`dep:name`.

Review

```toml
[features]
default = ["ssh"]
ssh = ["dep:foo-ssh", "foo-core/ssh"]
```

### 80. Declare `rust-version` on a published crate only

Test it in CI. An unpublished application pins `rust-toolchain.toml` instead.

Lint: `lint.sh` manifests

```toml
[package]
name = "foo-path"
rust-version = "1.85.0"
```

### 81. Use the latest stable edition for a new crate

The rule is relative on purpose, so it does not go stale at the next edition.

Lint: `lint.sh` manifests

```toml
[package]
edition = "2024"
```

## Tooling

Every check runs the same way locally and in CI.

### 82. Deny warnings everywhere and pin the compiler

The toolchain pin keeps a new compiler's new warning from breaking a build
nobody opted into.

Lint: rustc `warnings = "deny"`, clippy `-D warnings`

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.90.0"
```

### 83. Fix what a check flags, and suppress only a false positive

Suppress with a local `#[expect(lint, reason = "...")]`, never a bare
`#[allow]`. An `#[expect]` errors once the lint stops firing.

Lint: `clippy::allow_attributes`, `allow_attributes_without_reason`

```rust
// Good
#[expect(clippy::cast_possible_truncation, reason = "frame lengths are capped at 16 KiB")]
let len = frame.len() as u16;

// Bad
#[allow(clippy::cast_possible_truncation)]
let len = frame.len() as u16;
```

### 84. Run clippy on Linux and Windows

Run it on both `ubuntu-latest` and `windows-latest`, since Linux clippy never
sees `#[cfg(windows)]` code.

Review

```yaml
strategy:
  matrix:
    os: [ubuntu-latest, windows-latest]
```

### 85. Format with nightly rustfmt and the lint set's `rustfmt.toml`

Its unstable block makes the import rule mechanical. `cargo fmt --check` runs
as its own CI step.

Lint: `cargo +nightly fmt --check`

```toml
max_width = 100
newline_style = "unix"
indent_style = "Block"
use_field_init_shorthand = true

# Unstable features
unstable_features = true
imports_granularity = "Module"
group_imports = "StdExternalCrate"
reorder_impl_items = true
normalize_doc_attributes = true
```

### 86. Keep build steps in the project's scripts, and run CI once per commit

CI only checks out and invokes `.dev/<verb>.sh`. Trigger a suite on
`pull_request` for branches and on `push` for trunk and tags only.

Review

```yaml
on:
  pull_request:
  push:
    branches: [main]
    tags: ["v*"]
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: .dev/lint.sh
```

### 87. Look for an extraction point past about 40 lines

The number prompts a look and is never a finding on its own. A long `match` or a
builder is fine.

Review

```rust
// Good: each step is a named function
pub fn build(config: &Config) -> anyhow::Result<Document> {
    let fonts = load_fonts(config)?;
    let pages = render_pages(config, &fonts)?;
    Ok(Document::new(fonts, pages))
}
```

## mlua and the Lua boundary

These rules apply when `mlua` is in the manifest. The measurements behind them
are in the `style` skill's `references/rust-mlua.md`.

### 88. Cross a value type as a table with Luau methods, and a handle as `UserData`

A value type crosses as a plain table whose methods are written in Luau and
shipped with the embedded stdlib, never as Rust closures on a table. A handle to
Rust-owned state crosses as `UserData`, never as a copy.

The Rust call boundary is the cost: Luau methods run `v:add(w)` in 207 ns
against about 750 ns for Rust methods or `UserData`. A per-frame copy of state is
the slowest option and the only one that can drift.

Review

```lua
-- Shipped with the embedded stdlib
function Vec2:add(other)
    return Vec2.new(self.x + other.x, self.y + other.y)
end
```

### 89. Convert a value type with manual `IntoLua` and `FromLua`

`IntoLua` mirrors the value into a plain table plus metatable methods, and
`FromLua` parses leniently with missing fields defaulted.

Review

```rust
impl IntoLua for PdfObjectCircle {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let table = lua.create_table_ext(Some("pdf.object.circle"))?;
        table.raw_set("radius", self.radius)?;
        table.into_lua(lua)
    }
}
```

### 90. Put repeated boundary glue in extension traits

Pairing a table with its metatable, marking it read-only and naming the bad key
in an error each live in one helper.

Review

```rust
pub trait LuaExt {
    fn create_table_ext(&self, metatable: Option<&str>) -> LuaResult<LuaTable>;
}
```

### 91. Cross Rust state as app data or `Weak` handles

Give Lua read-only mirrors of the state, never the state itself.

Review

```rust
let fonts = lua.app_data_ref::<RuntimeFonts>().expect("fonts registered at startup");
```

### 92. Name the Lua-side type in a conversion error

Report the dotted Lua name, not the Rust type.

Review

```rust
// Good
let error = LuaError::FromLuaConversionError {
    from: value.type_name(),
    to: "pdf.object.circle".into(),
    message: None,
};

// Bad
let error = LuaError::FromLuaConversionError {
    from: value.type_name(),
    to: "PdfObjectCircle".into(),
    message: None,
};
```

### 93. Expose one global entry table

Name the global in `constants.rs`, and layer the Lua stdlib over it from
`include_bytes!` assets before the user script runs.

Review

```rust
lua.globals().set(GLOBAL_NAME, pdf_table)?;
lua.load(STDLIB_SOURCE).exec()?;
lua.sandbox(true)?;
```

### 94. Test boundary code in the guest language

Drive real Lua from Rust unit tests and assert on the Lua side.

Review

```rust
#[test]
fn circle_should_round_trip_through_lua() {
    let lua = test_runtime();
    lua.load(chunk! {
        local circle = pdf.object.circle({ radius = 4 })
        pdf.utils.assert_deep_equal(circle.radius, 4)
    })
    .exec()
    .unwrap();
}
```

### 95. Keep userdata out of table keys in a deterministic program

Luau hashes a userdata key by address, so `pairs` order over userdata keys
differs between processes.

Review

```lua
-- Good
local by_id = {}
by_id[entry:id()] = entry

-- Bad
local by_entry = {}
by_entry[entry] = true
```

## This project

Rules that apply only to this project go here.

### P1. Name a type without its module path

Name a type for the domain concept it holds (`Task`, `AuthUser`,
`CredentialKind`). This project is exempt from rule 16: its modules are few
and every type name is already unique in the crate.

Review

```rust
// src/models.rs
pub enum CredentialKind {
```

### P2. Keep the OpenAPI spec to the JSON API

Annotate only handlers an API client calls with `#[utoipa::path]`, and list
only them in `ApiDoc`. HTML pages and htmx fragments stay in the router and
out of the spec.

Lint: `src/routes/openapi.rs`

### P3. Put `operation_id` right after `path`

Every `#[utoipa::path]` sets an explicit snake_case `operation_id` on the line
after `path`, so renaming a handler leaves the contract unchanged.

Lint: `src/routes/openapi.rs` (presence and uniqueness), Review (order)

```rust
#[utoipa::path(
    get,
    path = "/api/tasks",
    operation_id = "list_tasks",
    tag = "Tasks",
```

### P4. Write plain backticks in docs that reach the spec

utoipa copies the `///` doc of a `ToSchema` type, its fields and an annotated
handler into the spec verbatim, so an intra-doc link reaches agents as raw
``[`Task`]`` text. Backtick the name there instead. This overrides rule 12 for
those docs only.

Review

```rust
/// A task as the API returns it, with its `Column` id.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
```
