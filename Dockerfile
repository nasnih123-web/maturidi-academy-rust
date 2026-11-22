# Maturidi Academy API — Rust (production image)
#
# Multi-stage build for the pure-Rust Axum server (Wave 8 lift: no proxy
# fallback, no Python runtime). The Quran data under rust/app/data/ is
# compiled into the binary with include_str!, so the runtime image only
# needs the binary plus a writable ./storage directory (local provider).
#
# Build context: the repository root (the Cargo workspace lives there).
# NOT yet built anywhere — no Docker daemon is available in this
# workspace, so the first Railway build is the first verification
# (see scripts/cutover-README.md). Kept minimal on purpose: the
# original's Python image needed ffmpeg/tesseract/poppler for the media
# toolchain; the Rust binary owns no media toolchain (ADR-137) and
# reports every tool unavailable, so none of those packages are
# installed here.

FROM rust:1.100-slim-bookworm AS build
WORKDIR /code
COPY Cargo.toml Cargo.lock ./
COPY rust ./rust
RUN cargo build --release -p maturidi-api-rs --locked

FROM debian:bookworm-slim AS runtime
RUN useradd -m -u 10001 appuser
WORKDIR /code
COPY --from=build /code/target/release/maturidi-api-rs ./maturidi-api-rs
RUN mkdir -p /code/storage && chown -R appuser:appuser /code
USER appuser
# The binary reads PORT itself (default 8000) and binds 0.0.0.0:$PORT.
# No HEALTHCHECK here on purpose — Railway uses healthcheckPath /health
# (railway.toml); the same image is not reused for workers.
CMD ["./maturidi-api-rs"]
