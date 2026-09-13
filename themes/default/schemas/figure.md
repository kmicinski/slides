## A figure in the theme's colours

<div class="figure narrow">
<svg viewBox="0 0 1200 200" xmlns="http://www.w3.org/2000/svg">
<defs><marker id="ex-accent" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="#2563eb"/></marker></defs>
<rect class="fig-box model" x="40" y="50" width="300" height="100" rx="14"/>
<text class="fig-title" x="190" y="110" text-anchor="middle">input</text>
<g class="fragment"><path class="fig-arrow model" marker-end="url(#ex-accent)" d="M345,100 L855,100"/>
<text x="600" y="85" text-anchor="middle">step <tspan class="sub">t</tspan></text></g>
<rect class="fig-box world" x="860" y="50" width="300" height="100" rx="14"/>
<text class="fig-title" x="1010" y="110" text-anchor="middle">output</text>
</svg>
</div>

Boxes: `fig-box` + `model` / `world` / `ctx`. Arrows: `fig-arrow` + the same modifiers; `fig-boundary` is the dashed red line. Text: `fig-title`, `fig-small`, `fig-mono`, `fig-accent` / `fig-heading` / `fig-accent2` / `fig-bad`. Wrap a `<g>` in `class="fragment"` to reveal it step by step.
