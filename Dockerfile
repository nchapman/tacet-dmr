# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM golang:1.27-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY cmd/ cmd/
COPY internal/ internal/
ARG TARGETOS TARGETARCH
RUN CGO_ENABLED=0 GOOS=$TARGETOS GOARCH=$TARGETARCH go build -tags nocgo -trimpath -ldflags='-s -w' -o /out/tacet-dmr ./cmd/tacet-dmr

# The runtime library and its transitive codec/output dependencies are needed
# even for -help. Keep the distro copyright notices in the final image.
FROM ubuntu:26.04 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libmpv2 && rm -rf /var/lib/apt/lists/*

# CI tests against the same distro's libmpv, not the runner host's older ABI.
FROM runtime AS check
COPY --from=build /usr/local/go /usr/local/go
COPY --from=ghcr.io/astral-sh/uv:0.12.14 /uv /usr/local/bin/uv
ENV PATH="/usr/local/go/bin:${PATH}"
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN sh scripts/check.sh

FROM runtime AS renderer
COPY --from=build /out/tacet-dmr /usr/local/bin/tacet-dmr
ENV HOME=/tmp
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/tacet-dmr"]
CMD ["-help"]
