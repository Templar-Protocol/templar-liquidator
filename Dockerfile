# Templar Liquidator Bot - standalone multi-stage Docker build.
#
# This is a single-crate repo (see the `[workspace]` note in Cargo.toml), so
# the build context is the repo root and only this crate's own files are
# copied in - no monorepo sibling crates.

# ============================================
# Build Stage
# ============================================
# Pinned by digest (not just tag) so the same source revision always
# produces the same image, even after upstream retags rust:1.97.0-bookworm
# or the Debian repository underneath it changes. Tag kept alongside the
# digest for readability; the digest is what's actually resolved. Matches
# rust-toolchain.toml's `channel = "1.97.0"`. To refresh deliberately:
#   docker buildx imagetools inspect rust:1.97.0-bookworm
# and update both the tag and the digest below together.
FROM rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073 AS builder

# git is already present in rust:*-bookworm and is required: the templar-*
# dependencies below are pulled from github.com/Templar-Protocol/contracts
# via git Cargo dependencies, so this stage also needs network access.
#
# nodejs + npm: templar-gateway-oracle-updates-dispatch (a direct dependency,
# used here only for its Pyth source - see src/service.rs's
# `.with_pyth_source`) unconditionally depends on templar-redstone-bridge,
# whose build.rs unconditionally shells out to `npm install`/`build`/`bundle`
# to produce a JS bundle embedded in the compiled binary. Same requirement as
# services/blockchain-gateway/Dockerfile in the backend monorepo, which
# depends on the same crate.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    nodejs \
    npm \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Only what a release build of the `liquidator` binary needs.
#
# Deliberately NOT copied:
#   - rust-toolchain.toml: pins channel "1.97.0" + the rustfmt/clippy
#     components for local dev and CI. The builder image above is already
#     pinned to the matching rustc 1.97.0, which is also Cargo.toml's
#     `rust-version` MSRV; adding rust-toolchain.toml here would make
#     rustup try to fetch components neither needed for `cargo build`, for
#     no benefit.
#   - .cargo/config.toml: sets CARGO_WORKSPACE_DIR, read via env!() only by
#     the templar-gateway-testing dev-dependency (used by tests/). `cargo
#     build --release --bin liquidator` never compiles dev-dependencies, so
#     it isn't needed for this build.
#   - clippy.toml: lint configuration, irrelevant to `cargo build`.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release --bin liquidator

# ============================================
# Runtime Stage
# ============================================
# Pinned by digest for the same reproducibility reason as the builder stage
# above. To refresh deliberately:
#   docker buildx imagetools inspect debian:bookworm-slim
# and update both the tag and the digest below together.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

# ca-certificates: TLS to NEAR RPC / oracle / notification endpoints.
# libssl3:         runtime counterpart of the builder's libssl-dev.
# procps:           provides `pgrep`, used by HEALTHCHECK below - not
#                   installed by default in bookworm-slim.
#
# Deliberately NOT installed: nodejs. templar-redstone-bridge's JS bundle is
# embedded into the binary at build time; running it needs a `node`
# subprocess only via `.with_redstone_source(...)`, which this binary never
# calls (it only calls `.with_pyth_source(...)` - see src/service.rs and the
# "not by the liquidator" note in src/oracle.rs). Unlike
# services/blockchain-gateway/Dockerfile in the backend monorepo, which does
# call it and installs `nodejs` at runtime for exactly that reason.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    procps \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash liquidator

WORKDIR /app

COPY --from=builder /app/target/release/liquidator /app/liquidator
COPY --chown=liquidator:liquidator .env.example ./.env.example

USER liquidator

ENV RUST_LOG=info,templar_liquidator=debug
ENV RUST_BACKTRACE=1

# Documentation only: EXPOSE neither publishes the port nor can interpolate
# the runtime HTTP_PORT env var. The optional /healthz + /metrics surface
# (src/http.rs) only listens when the operator sets HTTP_PORT, and 9090 is
# the value .env.example documents as the convention. Actual publishing
# happens in compose via `ports:`.
EXPOSE 9090

# Liveness check: is the `liquidator` process still running. Deliberately
# NOT wired to /healthz - that endpoint is a readiness signal (can the bot
# currently reach the chain?), not a liveness one, and restarting a process
# stuck because RPC is down doesn't fix RPC being down (see src/http.rs).
HEALTHCHECK --interval=60s --timeout=10s --start-period=10s --retries=3 \
    CMD ["pgrep", "-x", "liquidator"]

LABEL org.opencontainers.image.title="Templar Liquidator"
LABEL org.opencontainers.image.description="Inventory-based liquidation bot for Templar Protocol lending markets on NEAR"
LABEL org.opencontainers.image.vendor="Templar Protocol"
LABEL org.opencontainers.image.licenses="GPL-3.0-only"
LABEL org.opencontainers.image.source="https://github.com/Templar-Protocol/templar-liquidator"

ENTRYPOINT ["/app/liquidator"]
