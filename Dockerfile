# syntax=docker/dockerfile:1

FROM rust:1.90-bookworm AS build
ARG TARGETARCH
ARG VERSION=dev
RUN --mount=type=cache,id=apt-cache-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=apt-lists-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
        libasound2-dev pkg-config
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ENV TACET_ENDPOINT_VERSION=${VERSION}
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=cargo-target-release-${TARGETARCH},target=/src/target,sharing=locked \
    cargo build --release --locked --bin tacet-endpoint \
    && cp /src/target/release/tacet-endpoint /usr/local/bin/tacet-endpoint

FROM build AS check
ARG TARGETARCH
RUN --mount=type=cache,id=apt-cache-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=apt-lists-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-venv
COPY --from=ghcr.io/astral-sh/uv:0.12.14 /uv /usr/local/bin/uv
COPY interop/pyproject.toml interop/uv.lock ./interop/
RUN --mount=type=cache,id=uv-cache-${TARGETARCH},target=/root/.cache/uv,sharing=locked \
    uv sync --project interop --frozen --no-install-project
COPY interop ./interop
COPY scripts ./scripts
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=cargo-target-test-${TARGETARCH},target=/src/target,sharing=locked \
    --mount=type=cache,id=uv-cache-${TARGETARCH},target=/root/.cache/uv,sharing=locked \
    cargo test --workspace --locked \
    && TACET_ENDPOINT_BIN=/usr/local/bin/tacet-endpoint \
       uv run --project interop --frozen python -m unittest discover -s interop -v

FROM debian:bookworm-slim AS runtime
ARG TARGETARCH
RUN --mount=type=cache,id=apt-cache-rt-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=apt-lists-rt-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
        libasound2 ca-certificates \
    && groupadd --system tacet \
    && useradd --system --gid tacet --groups audio --no-create-home --shell /usr/sbin/nologin tacet
COPY --from=build /usr/local/bin/tacet-endpoint /usr/local/bin/tacet-endpoint
USER tacet
ENTRYPOINT ["/usr/local/bin/tacet-endpoint"]
