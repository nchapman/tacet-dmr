# syntax=docker/dockerfile:1

FROM rust:1.90-bookworm AS build
ARG VERSION=dev
ENV TACET_ENDPOINT_VERSION=${VERSION}
RUN apt-get update && apt-get install -y --no-install-recommends \
        libasound2-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --bin tacet-endpoint

FROM build AS check
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-venv \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.12.14 /uv /usr/local/bin/uv
COPY interop ./interop
COPY scripts ./scripts
RUN cargo test --workspace --locked \
    && TACET_ENDPOINT_BIN=/src/target/release/tacet-endpoint \
       uv run --project interop python -m unittest discover -s interop -v

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        libasound2 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system tacet \
    && useradd --system --gid tacet --groups audio --no-create-home --shell /usr/sbin/nologin tacet
COPY --from=build /src/target/release/tacet-endpoint /usr/local/bin/tacet-endpoint
USER tacet
ENTRYPOINT ["/usr/local/bin/tacet-endpoint"]
