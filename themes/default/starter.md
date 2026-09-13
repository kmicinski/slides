<!-- title: Deck Title Here -->
<!-- theme: default -->
<!-- .slide: class="title-slide" -->
<span class="course-tag">Course 101 &bull; Spring</span>

# Deck Title Here

## Topic subtitle &middot; Your Name

<div class="footer">course 101 &bull; lecture 1</div>

Note:
Speaker notes for the title slide. A line of three hyphens with blank lines
around it starts a new slide; two hyphens start a vertical sub-slide.
Every class used here is a schema of the theme: see themes/default/schemas/.

---

## A normal content slide

Write plain markdown. Bullets use `-`:

- First point
- Second point, with **strong** and *emphasis*
- Inline `code`, math $e^{i\pi} + 1 = 0$, and a [link](https://example.com)

> A blockquote renders as a highlighted callout.

---

## Code

```python
def fib(n: int) -> int:
    return n if n < 2 else fib(n - 1) + fib(n - 2)
```

--

## A vertical sub-slide

Reached by pressing "down." Use `--` to stack sub-slides under a point.

---

<!-- .slide: class="section-divider" -->
<span class="chapter-num">Section &bull; 01</span>

# Section Divider

## Use these to break the deck into chapters
