# Three stages: compile the browser code, build the server, ship a small image.
#
#   docker build -t slides .                          # editor + players + MCP
#   docker build --build-arg CLAUDE_CLI=1 -t slides . # also the ✦ Ask agent's `claude` CLI
#
# The agent is optional. Without a `claude` binary the editor simply has no
# ✦ Ask button. With CLAUDE_CLI=1 the official installer puts the CLI in the
# image; at run time give it either ANTHROPIC_API_KEY or a mounted OAuth
# credentials file (README, "Review mode and the in-app agent").

FROM node:22-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ .
RUN npx tsc

FROM rust:1-slim-bookworm AS build
# KaTeX runs in an embedded QuickJS, which is C; reqwest's TLS (aws-lc) builds with cmake.
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web/editor.css ./web/editor.css
COPY --from=web /web/dist ./web/dist
RUN cargo build --release

FROM debian:bookworm-slim
# git + certs: the in-app agent runs the `claude` CLI, which expects them.
# poppler-utils: pdftoppm / pdftotext / pdfinfo for the fetch_asset / pdf tools (src/assets.rs).
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates poppler-utils && rm -rf /var/lib/apt/lists/*
ARG CLAUDE_CLI=0
# The installer wants curl and a HOME; it lands a native binary under
# $HOME/.local/share/claude and a symlink in $HOME/.local/bin. Keep only the binary.
RUN if [ "$CLAUDE_CLI" = "1" ]; then \
      apt-get update && apt-get install -y --no-install-recommends curl \
      && HOME=/tmp/claude-install bash -c 'curl -fsSL https://claude.ai/install.sh | bash' \
      && cp -L /tmp/claude-install/.local/bin/claude /usr/local/bin/claude \
      && rm -rf /tmp/claude-install \
      && apt-get purge -y curl && apt-get autoremove -y && rm -rf /var/lib/apt/lists/* \
      && claude --version; \
    fi
RUN useradd -u 1000 -m slides
WORKDIR /app
COPY --from=build /build/target/release/slides /usr/local/bin/slides
COPY engine ./engine
COPY themes ./themes
RUN mkdir decks && chown -R slides:slides /app
USER slides
ENV SLIDES_ROOT=/app SLIDES_BIND=0.0.0.0:7100 HOME=/home/slides
EXPOSE 7100
CMD ["slides"]
