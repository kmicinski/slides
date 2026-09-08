<!-- title: CIS400 — Deck Title Here -->
<!-- theme: cis400 -->
<!-- .slide: class="title-slide" -->
<span class="course-tag">CIS 400 &bull; Syracuse University</span>

# Deck Title Here

## Topic subtitle &middot; Prof. Kristopher Micinski

<div class="footer">cis400 &bull; cybersecurity &amp; ai</div>

Note:
Speaker notes for the title slide. A line of three hyphens with blank lines
around it starts a new slide; two hyphens start a vertical sub-slide.
Every class used here is a schema of the theme: see themes/cis400/schemas/.

---

## A normal content slide

Write plain markdown. Bullets use `-`:

- First point
- Second point, with **strong** and *emphasis*
- Inline `code`, math $x' = x + \varepsilon\,\mathrm{sign}(\nabla_x J)$, and a [link](https://example.com)

> A blockquote renders as a highlighted callout.

---

## Code + a demo link

```python
def is_vulnerable(payload: str) -> bool:
    return "'; DROP TABLE" in payload
```

<a class="playground" target="_blank" href="https://example.com">&#9654; Open demo</a>

--

## A vertical sub-slide

Reached by pressing "down." Use `--` to stack sub-slides under a point.

---

<!-- .slide: class="section-divider" -->
<span class="chapter-num">Section &bull; 01</span>

# Section Divider

## Use these to break the deck into chapters
