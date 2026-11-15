# syntax=docker/dockerfile:1
# graphbuff-server (and the graphbuff shell) without a Rust toolchain:
#
#   docker build -t graphbuff .
#   docker run --rm -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 graphbuff --demo movies
#
# The build stage runs on the build machine's own architecture and cross-compiles for
# the target platform, so `docker buildx build --platform linux/amd64,linux/arm64` needs
# no emulation.

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
WORKDIR /src
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu > /target-triple ;; \
      arm64) echo aarch64-unknown-linux-gnu > /target-triple ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && rustup target add "$(cat /target-triple)" \
 && if [ "$(cat /target-triple)" != "$(rustc -vV | sed -n 's/^host: //p')" ]; then \
      apt-get update \
   && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu gcc-x86-64-linux-gnu libc6-dev-arm64-cross libc6-dev-amd64-cross \
   && rm -rf /var/lib/apt/lists/*; \
    fi
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY examples/movies.cypher examples/movies.cypher
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=graphbuff-target-$TARGETARCH \
    cargo build --release --locked --target "$(cat /target-triple)" -p graphbuff-cli -p graphbuff-server \
 && mkdir -p /out/bin /out/data \
 && cp "target/$(cat /target-triple)/release/graphbuff" "target/$(cat /target-triple)/release/graphbuff-server" /out/bin/

# glibc, libgcc and CA certificates only; runs as the unprivileged user `nonroot` (65532).
FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="graphbuff" \
      org.opencontainers.image.description="graphbuff graph database: Cypher over HTTP/JSON and Bolt" \
      org.opencontainers.image.source="https://github.com/JanVencl/graphbuff" \
      org.opencontainers.image.licenses="AGPL-3.0-only"
COPY --from=build /out/bin/ /usr/local/bin/
COPY --from=build --chown=65532:65532 /out/data /data
USER nonroot
WORKDIR /data
VOLUME /data
# HTTP/JSON and Bolt. The server listens on every interface of the container; which
# machines can reach it is decided by `docker run -p`.
EXPOSE 7475 7687
ENTRYPOINT ["graphbuff-server", "--bind", "0.0.0.0:7475", "--bolt", "0.0.0.0:7687"]
