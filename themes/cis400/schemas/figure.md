## A figure in the theme's colours

<div class="figure narrow">
<svg viewBox="0 0 1200 200" xmlns="http://www.w3.org/2000/svg">
<defs><marker id="ex-orange" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="#f76900"/></marker></defs>
<rect class="fig-box model" x="40" y="50" width="300" height="100" rx="14"/>
<text class="fig-title" x="190" y="110" text-anchor="middle">model</text>
<g class="fragment"><path class="fig-arrow model" marker-end="url(#ex-orange)" d="M345,100 L855,100"/>
<text x="600" y="85" text-anchor="middle">action a<tspan class="sub">t</tspan></text></g>
<rect class="fig-box world" x="860" y="50" width="300" height="100" rx="14"/>
<text class="fig-title" x="1010" y="110" text-anchor="middle">world</text>
</svg>
</div>

Boxes: `fig-box` + `model` / `world` / `ctx`. Arrows: `fig-arrow` + the same modifiers; `fig-boundary` is the dashed red line. Text: `fig-title`, `fig-small`, `fig-mono`, `fig-orange` / `fig-navy` / `fig-signal` / `fig-danger`. Wrap a `<g>` in `class="fragment"` to reveal it step by step.
