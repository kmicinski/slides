# Three stages: compile the browser code, build the server, ship a small image.

FROM node:22-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ .
RUN npx tsc

FROM rust:1-slim-bookworm AS build
# KaTeX runs in an embedded QuickJS, which is C.
RUN apt-get update && apt-get install -y --no-install-recommends build-essential && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web/editor.css ./web/editor.css
COPY --from=web /web/dist ./web/dist
RUN cargo build --release

FROM debian:bookworm-slim
# git + certs: the in-app agent runs the (bind-mounted) `claude` CLI, which expects them.
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd -u 1000 -m slides
WORKDIR /app
COPY --from=build /build/target/release/slides /usr/local/bin/slides
COPY engine ./engine
COPY themes ./themes
RUN mkdir decks && chown -R slides:slides /app
USER slides
ENV SLIDES_ROOT=/app SLIDES_BIND=0.0.0.0:7100
EXPOSE 7100
CMD ["slides"]
