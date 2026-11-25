# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS build
ARG VERSION=dev
ENV TACET_DMR_VERSION=${VERSION}
RUN apt-get update && apt-get install -y --no-install-recommends \
        libasound2-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --bin tacet-dmr

FROM build AS check
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-venv \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.12.14 /uv /usr/local/bin/uv
COPY interop ./interop
COPY scripts ./scripts
RUN cargo test --workspace \
    && DMR_TEST_BIN=/src/target/release/tacet-dmr \
       uv run --project interop python -m unittest discover -s interop -v

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        libasound2 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system tacet \
    && useradd --system --gid tacet --groups audio --no-create-home --shell /usr/sbin/nologin tacet
COPY --from=build /src/target/release/tacet-dmr /usr/local/bin/tacet-dmr
USER tacet
ENTRYPOINT ["/usr/local/bin/tacet-dmr"]
