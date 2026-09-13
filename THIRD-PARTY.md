# Third-party code

The `engine/` directory vendors the presentation runtime so that an exported
deck is a self-contained folder. Nothing in it is modified.

| Component | Version | License | Where |
|---|---|---|---|
| [reveal.js](https://revealjs.com) (core, `highlight` and `notes` plugins) | 5.2.0 | MIT, © Hakim El Hattab | `engine/reveal/` |
| [highlight.js](https://highlightjs.org) (bundled in reveal's highlight plugin) | as shipped with reveal 5.2.0 | BSD-3-Clause | `engine/reveal/plugin/highlight/` |
| [KaTeX](https://katex.org) (CSS + fonts; the renderer runs server-side via the `katex` crate) | 0.16.4 | MIT | `engine/katex/` |
| [Inter](https://rsms.me/inter/) | variable | SIL Open Font License 1.1 | `themes/*/fonts/` |
| [JetBrains Mono](https://www.jetbrains.com/lp/mono/) | Regular, SemiBold | SIL Open Font License 1.1 | `themes/*/fonts/` |

The editor page loads [Monaco](https://microsoft.github.io/monaco-editor/)
0.45.0 (MIT) and `monaco-emacs` 0.3.0 (MIT) from public CDNs at page load;
they are not vendored. Rust and npm dependencies carry their own licenses in
`Cargo.lock` and `web/package-lock.json`.
