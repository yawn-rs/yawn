<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/logo-dark.svg">
    <img src="assets/logo/logo.svg" alt="Yawn" width="360">
  </picture>
</p>

<p align="center"><strong>A full-stack web framework for Rust: <code>.rsx</code> components, fine-grained reactivity, server-side rendering.</strong></p>

<p align="center">
  <img alt="License: MIT or Apache-2.0" src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue">
  <img alt="Status: early" src="https://img.shields.io/badge/status-early-orange">
</p>

<p align="center">
  <a href="examples/README.md">Examples</a> ·
  <a href="crates/yawn-rsx/README.md">.rsx guide</a> ·
  <a href="crates/yawn-router/README.md">Router</a> ·
  <a href="crates/yawn-reactivity/README.md">Reactivity</a> ·
  <a href="editors/README.md">Editors</a> ·
  <a href="bench/README.md">Benchmarks</a>
</p>

```rust
// src/counter.rsx
component Counter() {
    let count = signal(0i32);
    let double = memo(move || count.get() * 2);

    view {
        <button on:click={move || count.update(|c| *c += 1)}>"Clicked {count} times"</button>
        <p>"double = {double}"</p>
        if count.get() >= 10 { <p>"that's a lot"</p> }
    }
}
```

Components are written in `.rsx`: ordinary Rust next to declarative markup. The build step turns
them into plain Rust files you can read.

> **Early stage.** The core works, but APIs will change and a few pieces are still missing (see
> [Status](#status)).

## Features

- **Fine-grained reactivity.** Signals, memos and effects bind to single text nodes, attributes,
  classes, `if` branches and keyed `for` rows.
- **Server-side rendering with hydration.** The server renders the page, the WASM client picks it up.
- **Folder-based routing.** `src/routes/users/[id].rsx` is `/users/:id`, with nested layouts,
  `<Outlet />`, `<Link />` and a `preload` step that loads data before a page shows.
- **Fast feedback.** `yawn dev` rebuilds on every save. Stylesheets swap live, text and attribute
  edits in a view update the page in place, and after any other Rust edit the new client replaces the
  running one with the values of its signals carried over (numbers, booleans, strings, options and
  string lists). Compiler errors show their `.rsx` location in the terminal and the browser.
- **API routes.** A file in `src/api/` is an HTTP endpoint written as an axum handler, with no
  registration, and `yawn::http` calls it from the client.
- **Server functions.** A `fn` marked `#[get]`, `#[post]`, `#[put]`, `#[patch]` or `#[delete]` in a
  `.rsx` file runs on the server; the client calls it like any other async function and gets the
  typed result back.
- **Production builds.** `yawn build` writes `dist/` with content-hashed file names and
  `Cache-Control: immutable` for everything the browser can keep.
- **Static sites.** `yawn build --static` renders the pages to `.html` for a static host: every
  route in `src/routes/` without parameters, plus the paths in `[build] prerender` of `Yawn.toml`
  (`prerender = ["/users/1"]`). The data the pages load is saved next to them. It runs the
  project's own server during the build to render them.
- **Tailwind built in.** Tailwind v4 through its standalone executable.
- **Editor support.** A language server for `.rsx` (rust-analyzer behind a source map, plus HTML,
  CSS and Tailwind completion), a tree-sitter grammar and a Zed extension.
- **A small, safe core.** `yawn-reactivity` has no dependencies and `#![forbid(unsafe_code)]`.

## Quick start

You need Rust with the `wasm32-unknown-unknown` target. `yawn` uses the `wasm-bindgen` CLI on your
`PATH` when it matches the project, and otherwise downloads the matching release once into a cache.

```sh
rustup target add wasm32-unknown-unknown
cargo binstall yawn-cli                       # the `yawn` command; `cargo install yawn-cli` compiles it instead

yawn new my-app --tailwind
cd my-app
yawn dev                                      # http://127.0.0.1:3000
```

| command | does |
|---|---|
| `yawn dev` | builds the client and the server, serves, watches, reloads (`--port` picks the port) |
| `yawn build` | production build into `dist/`; run the binary from there (`--static` writes HTML for a static host into `dist/public`) |
| `yawn new <dir>` | creates a project (`--tailwind` turns Tailwind on); `yawn init` is the same command |
| `yawn upgrade` | installs the latest release of `yawn` (`--check` only says whether there is one) |
| `yawn rsx` | compiles the `.rsx` files and prints errors only |
| `yawn info` | shows the environment Yawn sees |

A project to read and run is in [`examples/`](examples/README.md).

## Performance

Measured on one machine (i5-12400F), reactivity only, on a 15-scenario suite: by geometric mean of
time ratios about 13x faster than `dioxus-signals 0.7.10` and about 6x faster than
`sycamore-reactive 0.9.4`. Memo and effect nodes use 3-5x less memory than in Dioxus (a bare signal
takes about 17% more). Method, caveats and every number are in [`bench/results.md`](bench/results.md).

## Status

| works | not yet |
|---|---|
| `.rsx` components (generic ones too), signals, memos, effects, keyed lists, slots, prop defaults | `match`, `while` and `loop` in a view |
| SSR with hydration, head tags, data loading | CSS scoped to a component (`style { }` is global) |
| folder routing with layouts and params | a stable API |
| API routes in `src/api/`, server functions | |
| `yawn dev` with state kept across rebuilds, `yawn build`, Tailwind, hashed assets | |
| language server, tree-sitter grammar, Zed extension | |

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo clippy -p yawn-web --target wasm32-unknown-unknown     # the browser backend
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo run --release -p yawn-reactivity --example perf
```

```sh
cargo install --path crates/yawn-cli                         # the `yawn` command from this checkout
yawn new ../my-app --yawn-path ./crates/yawn-web              # a project that builds against it
```

A change to the reactivity API needs a semantics test in `crates/yawn-reactivity/tests/`.

To release, set the new version in `Cargo.toml` (`[workspace.package]` and the three entries of
`[workspace.dependencies]`), commit, tag `vX.Y.Z` and push the tag. The `release` workflow builds the
`yawn` binaries that `cargo binstall` downloads, attaches them to the GitHub release and publishes
the crates.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and pull requests are welcome.

<a href="https://github.com/yawn-rs/yawn/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=yawn-rs/yawn&max=30&columns=10" />
</a>

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. Unless you state otherwise, any contribution you submit for inclusion is dual
licensed as above, without any additional terms.

The logo combines the Fluent Emoji (MIT) and the Baloo 2 font (OFL); see
[`assets/logo/NOTICE`](assets/logo/NOTICE).
