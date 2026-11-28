# yawn-rsx

The `.rsx` compiler: turns `.rsx` files (real Rust + declarative markup) into Rust that drives the
fine-grained renderer in `yawn::ui`. No proc macro: it's an ordinary build step, so
generated code is a normal file you can read (`.yawn/rsx/`), errors keep their `.rsx`
location, and `yawn dev` runs it before every client build.

## The format
```rust
// src/counter.rsx  ->  module `crate::counter`
use crate::stepper::Stepper;          // everything outside `component` is plain Rust, copied as is

component Counter() {                 // props are fields: `component Card(title: &'static str, children: Children)`
    let count = signal(0i32);         // setup: real Rust, runs once when the component is created
    let double = memo(move || count.get() * 2);
    effect(move || set_title(&format!("Count: {count}")));

    view {
        <p class="count" class:big={count.get() >= 10}>{count}</p>
        <p>"double = {double}"</p>
        <button on:click={move || count.update(|c| *c += 1)}>"+"</button>
        <Stepper step={step} />
        if count.get() >= 10 { <p>"a lot"</p> } else { <p>"keep going"</p> }
        <ul>
            for item in items.get() key item.id { <li>"{item.name}"</li> }
        </ul>
    }
}
```

| construct | meaning |
|---|---|
| `<tag attr="text">` | HTML element; void tags (`input`, `br`, ...) need no closing tag |
| `<Name prop={expr} flag />` | component: a struct literal `Name { prop: expr, flag: true }`, rendered once. A string value goes through `Into`, so `title="x"` fills a `String` or `&str` |
| `component List<T: Clone>(items: Vec<T>)` | generic component: the props the caller passes decide `T` (`<List items={vec![1, 2]} />`), so each parameter has to be in a prop's type. Bounds go inside `<>` or in `where` after the props, and a parameter takes no default (`<T = u8>`). Its code is compiled where it's rendered, so in `yawn dev` an edit is swapped in place only when no other file uses the component; otherwise the app is rebuilt |
| `"text {expr} {n:03} {{"` | string; `{expr}` interpolates (`format!` specs allowed). Without `{}` it's static |
| `{expr}` | a child: numbers, strings, signals/memos become a text node that follows them; a `View` or `Children` is inserted once |
| `name="x {expr}"`, `name={expr}` | attribute; reactive when it reads signals. `bool` sets/removes a valueless attribute, `Option` `None` removes it |
| `on:event={handler}` | listener; `\|\| ...` or `\|e: Event\| ...`; writes inside are batched; removed with its owner |
| `class:name={cond}` | toggles one class |
| `prop:name={expr}` | sets a DOM property (e.g. an input's `value`) |
| `ref={el}` | fills a `NodeRef` (`let el = node_ref();`) with the element: empty before it exists and after it's gone, with `el.rect()`, `el.focus()`, `el.blur()`, `el.scroll_into_view()` and, in the browser, `el.js_value()` |
| `if cond { } else if cond { } else { }` | conditional block; rebuilt only when the condition flips |
| `if let PAT = value { } else { }` | like `if`, with the pattern's bindings in scope; rebuilt whenever `value` changes (so its type needs `Clone + PartialEq`), not only when the pattern starts or stops matching |
| `for pat in expr key keyexpr { }` | keyed list: rows are built once per key, moved (not rebuilt) on reorder, disposed on removal |
| `style { css }` | plain CSS in the component (after or before `view`), global, written to `/pkg/rsx.css` which the dev server links automatically |
| `#[post] fn name(arg: T) -> R { }` | outside components: runs on the server, the client calls it like an async function; `#[get]`, `#[put]`, `#[patch]` and `#[delete]` choose the HTTP method (see [Server functions](#server-functions)) |
| `name: Class` (prop) | `Class` (`Prop<String>`) takes `class="x"`, `class="a {x}"` and `class={sig.into()}` (a signal or memo); read it as `{class}` in a view and the page follows the signal. `.and("b")` and `.when(|| cond, "c")` join names. `Prop<T>` is the same for any type; a `&'static str` prop takes literals only. Interpolated text props go through `Into`, `{expr}` props don't: write `.into()` |
| `<slot />` | where a component with a `children: Children` prop places what was written between its tags. `<slot name="x" />` places the prop `x: Children`, which the caller fills with `slot="x"` on a child (`<b slot="x">`) or with the contents of `<template slot="x">`; anything without a `slot` goes to `children`. A slot is built each time it's placed, so one inside `if` follows the branch (captured Strings must be signals, as in any branch) |

Not supported (clear diagnostics where it can be detected): `match`/`while`/`loop` in a view
(compute it in setup and write `{value}`), text without quotes, a `<style>`
scope (`style { }` is global), `async`.

## Architecture
```text
.rsx source
  -> scan.rs      Rust-aware tokens (strings, chars, lifetimes, comments, delimiters): finds the
                  extent of every Rust fragment and reports unclosed / mismatched delimiters
  -> parser.rs    file = Rust chunks + components; view = nodes (element, text, {expr}, if, for, slot).
                  Every Rust fragment (setup, expressions, conditions, patterns, params) is then
                  validated by `syn`, with errors mapped back to the exact `.rsx` span
  -> ast.rs       nodes refer to the source by byte range, nothing is copied
  -> codegen.rs   struct + `impl Component`; setup copied verbatim; the view becomes straight-line
                  code: create each node once, bind dynamic parts to closures (`bind_attr`,
                  `text_fmt`, `show`, `each`, `listen`, ...). Writes a SourceMap
  -> project.rs   every `src/**/*.rsx` -> `.yawn/rsx/**.rs` (+ `.map`) and `modules.rs`
                  (declares `crate::counter`, `crate::ui::card`); files are only rewritten when
                  their content changes, so cargo stays fresh
```
The generated code only calls `yawn::ui::rt`, which talks to a `Backend` trait (browser DOM,
in-memory tree for tests). Nothing in it is tied to the browser, so the same components can later be
rendered to a string on the server.

**Source maps.** Rust fragments are copied byte for byte, so a rustc error inside `{expr}` or setup
code maps to the same line and column of the `.rsx` file (verbatim runs map column by column; the
glue lines map to the construct they belong to). `yawn dev` rewrites cargo's diagnostics with
these maps, so type errors show up at `src/counter.rsx:47:65`, in the terminal and in the browser.

**Loop variables.** A `for` row closure owns its item; every closure inside the row that uses the item
(handlers, bindings, nested blocks) gets its own `Clone`, so `on:click={move || remove(item.id)}`
just works. Items must be `Clone`.

## Use it
**Entry point.** `src/main.rsx` is the application's root: the component named `App` is what the browser
mounts, and everything outside a component is plain Rust (`use`, `mod helpers;` which finds
`src/helpers.rs`). `yawn dev` / `yawn rsx` generate the crate roots from it into `.yawn/` (`client.rs`: the WASM
library with the `main.rsx` code, the other `.rsx` modules and the mount of `App`; `server.rs`: the
server) together with a copy of your `Cargo.toml` that points `[lib]` and `[[bin]]` at them; cargo is run
on that copy. Your `src/` holds only your own files and your `Cargo.toml` needs nothing beyond the
`yawn-web` dependency. Only `yawn dev` / `yawn rsx` build such a project (a plain `cargo build` finds no
targets). A top-level `mod helpers;` in `main.rsx` is pointed at `src/helpers.rs` (or `src/helpers/mod.rs`);
a file `src/x.rs` with child modules in `src/x/` isn't supported there, use `src/x/mod.rs`. Without a
`main.rsx` the hand-written `src/lib.rs` (`yawn::rsx_modules!();`, `yawn::client!(counter::Counter);`)
and `src/main.rs` keep working as before, with `crate-type = ["cdylib", "rlib"]` in `Cargo.toml`.

**Routes.** `src/routes/` is turned into a route table (Next.js style) by default; see
`crates/yawn-router/README.md`. `[router] folders = false` in `Yawn.toml` turns it off.

`yawn dev` compiles `.rsx` on
every client build; `yawn rsx` only compiles and reports errors. A plain `cargo build` needs one of
them to have run first (the modules file lives in `.yawn/rsx/`).

## Tests
`tests/compile.rs`: codegen shape, diagnostics (message + position), source maps. The sibling crate
`yawn-rsx-tests` compiles real `.rsx` fixtures at build time and renders them natively against the
in-memory document: reactivity, events, conditions, keyed lists (reuse and moves), nested components,
slots, disposal of effects, listeners and nodes.

## API routes (`src/api/`)

Files in `src/api/` become HTTP endpoints, as `pages/api` does in Next.js. They're plain Rust,
compiled for the server only, and need no registration.

| file                     | endpoint             |
|--------------------------|----------------------|
| `api/health.rs`          | `/api/health`        |
| `api/index.rs`           | `/api`               |
| `api/users/[id].rs`      | `/api/users/{id}`    |
| `api/files/[...path].rs` | `/api/files/{*path}` |
| `api/(admin)/stats.rs`   | `/api/stats`         |

A file defines one handler per HTTP method it answers: `pub async fn get`, `post`, `put`, `patch`,
`delete`, `head`, `options`. They're axum handlers (`yawn::server::axum`), so extractors work:

```rust
// src/api/users/[id].rs
use yawn::server::axum::{extract::Path, Json};

pub async fn get(Path(id): Path<String>) -> Json<String> {
    Json(id)
}
```

A file with no handler, a name that isn't a valid route, and two files that make the same route
are errors. The endpoints are merged into the app before `server::configure`, which stays for
middleware, state and anything else that isn't a file.

## Server functions

A `fn` marked `#[get]`, `#[post]`, `#[put]`, `#[patch]` or `#[delete]` in any `.rsx` file runs on the
server, and the client calls it like any other async function:

```rust
// src/todos.rsx  ->  crate::todos::add
#[post]
fn add(title: String) -> usize {
    crate::server::db().insert(title)
}

component NewTodo() {
    let count = signal(0);
    view {
        <button on:click={move || spawn(add("milk".into()), move |r| count.set(r.unwrap_or(0)))}>"add"</button>
    }
}
```

The compiler writes two functions with the signature `pub async fn add(title: String) ->
Result<usize, yawn::fns::Error>`: the server's runs the body, the client's sends the arguments to
`/__yawn/fn/todos/add` with the method of the attribute and returns the answer. Both return
`Result`, so a component that calls one compiles for SSR too; on the server the call is a direct
call and never fails. The server binary routes every server function of the project. `pub` and
`async` may stand before `fn`; the function is public either way.

- `#[get]` is for reads: the arguments go in the URL as `?args=`, the answer can be cached like
  any `GET`, and the call carries an `x-yawn-fn` header. A `GET` must not change anything.
- `#[post]`, `#[put]`, `#[patch]` and `#[delete]` send the arguments in the body.
- Extractors: `#[post(headers: HeaderMap, session: Session)]` names what the body takes from the
  request, any axum `FromRequestParts<()>` type. They are bound as variables in the body and are
  not arguments of the function, so the caller (and the client's version) doesn't see them. Their
  types appear only in the server's version of the function, so write them as full paths
  (`yawn::server::axum::http::HeaderMap`) or import them behind
  `#[cfg(not(target_arch = "wasm32"))]`. A call that didn't come over HTTP has no request and gets
  an error from the extractor.
- `#[head]` and `#[options]` are refused: a HEAD answer has no body to return, and OPTIONS is the
  browser's own preflight request.

### The wire format

Arguments are `name: Type` and the answer is the return type. By default both are written with
`yawn::bits`: positional (no field names), as small as the data allows, and decoded by straight-line
code. The types are the integers, `bool`, `char`, `f32`/`f64`, `String`, `Vec`, `Option`, `Result`,
`Box`, tuples, arrays and the standard maps and sets, and your own structs and enums with
`#[derive(Bits)]`:

```rust
use yawn::bits::Bits;

#[derive(Bits)]
struct Todo { id: u64, title: String, tags: Vec<String> }

#[post]
fn add(title: String) -> Todo { /* ... */ }
```

A message carries a version byte and a hash of the type, so a page older than the server (or the
other way round) gets `yawn::fns::Error::Outdated` instead of misread data: reload it. The decoder
is bounded: values nest at most 64 deep and every length it reads is checked against what is left
of the message, so a hostile body cannot overflow the stack or reserve memory it does not carry.
The derive refuses what a positional format cannot write (`serde(flatten)`, `untagged`, `tag`,
`skip_serializing_if`, `default`...) with a compile error that says what to do instead.

Another encoding is an option of the attribute, `#[post(encoding = json)]`, and takes `serde`
types:

| `encoding =` | what | needs |
|---|---|---|
| `bits` (default) | `Bits` types, positional, bounded, hash of the type | nothing |
| `json` | readable by anything, for a function called from outside yawn | feature `json` |
| `postcard` | `serde` types, as small as `bits`; no type hash, no limit on nesting | feature `postcard` |
| `dynamic` | `serde` types in a self-describing binary format (`flatten`, `untagged`, `serde_json::Value`), bounded | feature `dynamic` |

- The calls a page makes in the same tick are sent as **one request** to `/__yawn/batch`: the
  server runs them in order, the reads between two writes at once, and answers each by itself (one
  failing does not fail the others). A lone call is an ordinary request. Nothing in the code
  changes. A batch has at most 32 calls from the client (64 are accepted).
- **Pipelining.** A function with the default encoding also has a `name_p` twin that returns a
  `Pending<T>` (await it for the answer) and takes, for each argument, a value, a whole earlier
  `&Pending`, or a piece of one with `pick`. The server runs the calls in order and puts the piece
  of the answer where the next call needs it, so a chain costs one round trip:

  ```rust
  let user = open_account_p("ann".to_string());      // nothing sent yet
  let label = label_p(user.pick(|a| a.id), "id-".to_string());
  let label = label.await;                           // one request carried both calls
  ```

  `pick` takes the fields `#[derive(Bits)]` writes for a struct (`user.pick(|u| u.profile).pick(|p|
  p.age)`), so a field that does not exist is a compile error. A piece is good for the tick it was
  made in (in the browser). While rendering on the server the call runs where it is and the piece is
  cut from the answer of the earlier one, so the same code works in both.
- **WebSocket.** With the `yawn-web` features `ws` (server) and `socket` (browser), batches travel
  over one socket the page opens on first use at `/__yawn/ws`. The extractors (session, headers)
  run against the upgrade request, which is checked once, and the browser skips a request's
  headers and handshake each time. A socket that is not there, was closed or timed out falls back
  to fetch with the same batch. The upgrade is refused (403) when its `Origin` is not the host, so
  another site cannot ride on the user's cookies.
- The body is async (`.await` works) and compiled for the server only. Imports it alone needs go
  inside it, or behind `#[cfg(not(target_arch = "wasm32"))]`: the rest of the file is compiled for
  the client as well.
- The server answers a `#[get]` only with the `x-yawn-fn` header and the others only with the
  content type of their encoding (`application/vnd.yawn.bits`, `application/json`...); a
  cross-site page can send neither, so it can't call a function with the user's cookies.
- Arguments come from the browser: validate them as you would any request.

Not supported yet: generics, argument patterns (`(a, b): (u8, u8)`), streaming, and defining one in
a plain `.rs` file.
