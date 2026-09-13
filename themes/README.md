# Making a theme

A theme is a directory under `themes/`. Nothing in it is compiled: the player
links the stylesheets directly and `export.zip` copies the directory as-is.
The fastest way to a new theme is to copy `default` and change the palette.

```
cp -r themes/default themes/acme
```

Then, in `themes/acme/`:

1. **`base.css` — the palette.** Every colour in the theme comes from the
   `:root` block at the top of this file. Change those hex values and the whole
   theme follows, schemas included, because the schemas only ever use the
   variable names:

   | Variable | Used for |
   |---|---|
   | `--accent`, `--accent-deep`, `--accent-soft` | h1, the rule under h2, bullets, links, table headers, the slide number |
   | `--accent2` | the second colour of the top bar and progress gradients, "info" figure boxes |
   | `--heading`, `--heading-soft` | h2/h4, the title slide's subtitle, emphasis |
   | `--text`, `--ink`, `--muted` | body text, callout text, footers and captions |
   | `--good`, `--warn`, `--bad` | callout variants and figure lines |
   | `--page`, `--page-soft`, `--rule` | slide background, blockquote/table stripes, borders |
   | `--code-bg` | inline and block code background (match `highlight.css`) |

   The rest of `base.css` styles what markdown produces on its own (headings,
   lists, code, tables, reveal's controls). Type sizes and fonts live here too.

2. **`fonts/`** — referenced from `base.css` by relative URL (`fonts/…woff2`).
   Swap the files and the `@font-face` blocks together, or delete both and
   name a system font in the `font-family` rules.

3. **`highlight.css`** — code colours (highlight.js classes). Any highlight.js
   theme drops in; keep `--code-bg` in `base.css` in step with its background.

4. **`theme.toml`** — the `[reveal]` table is passed to `Reveal.initialize()`
   untouched, so any option from <https://revealjs.com/config/> goes here:
   slide size, transition, whether the slide number shows.

5. **`starter.md`** — what a new deck starts from. Put `<!-- theme: acme -->`
   on its second line so decks created from it stay on your theme, and make it
   show off the schemas you kept.

6. **`schemas/`** — one CSS file plus one markdown example per slide design an
   author writes by hand (`<!-- .slide: class="title-slide" -->`,
   `<div class="callout">`, …). Keep, drop, or add pairs freely: the file
   name is the class name, the `.md` is the example the editor and the API
   hand to tools so they know your vocabulary. A schema with no `.md` still
   works, but tools will not know how to use it.

Restart the server (or, in Docker, nothing — `themes/` is bind-mounted and
read on demand) and the theme appears in the new-deck form. A deck opts in
with `<!-- theme: acme -->` at the top; a deck that names none gets the
default theme (`SLIDES_DEFAULT_THEME`, else the one called `default`). Reload
the editor page after editing a theme; players pick up changes on refresh.

To add a slide design to an existing theme without touching files, tools can
write schemas over the API or MCP (`put_schema`): CSS plus example in, file
pair out.

`cis400` is a complete branded example of all of this — Syracuse orange and
navy, extra schemas (`stat`, `source`, `playground`, `trace`), course-specific
starter — built the same way.
