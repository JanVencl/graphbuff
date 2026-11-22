# syntax=docker/dockerfile:1
# graphbuff-server (and the client graphbuff-cli) without a Rust toolchain:
#
#   docker build -t graphbuff .
#   docker run --rm -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
#     -e GRAPHBUFF_ADMIN_PASSWORD=change-me graphbuff
#
# Settings are in /etc/graphbuff/graphbuff.toml (packaging/docker/graphbuff.toml in the
# repository). To change them, mount your own copy over it:
#
#   docker run ... -v ./graphbuff.toml:/etc/graphbuff/graphbuff.toml:ro graphbuff
#
# Clients log in with user accounts, kept in the system database in /data. On the first
# start (no administrator yet) the server creates the administrator GRAPHBUFF_ADMIN_USER
# (default: admin) with the password in GRAPHBUFF_ADMIN_PASSWORD, or in the file named by
# GRAPHBUFF_ADMIN_PASSWORD_FILE (a Docker secret: /run/secrets/NAME). Without them it asks
# when run with `-it`, and otherwise stops. `-e GRAPHBUFF_TOKEN=...` adds a token that
# logs in as an administrator.
#
# Offline, while no server uses the volume (the image has no shell, so name the binary):
#
#   docker run --rm -it -v graphbuff-data:/data --entrypoint graphbuff-server graphbuff admin init
#   docker run --rm -it -v graphbuff-data:/data --entrypoint graphbuff-server graphbuff \
#     admin reset-password admin
#
# Embedding models for vector.embed() go in /models, one sub-folder per model (see
# packaging/model/README.md); mount a folder of them read-only:
#
#   docker run ... -v ./model:/models:ro graphbuff
#
# The client runs inside the server's container (the server's own address and login):
#
#   docker exec -it CONTAINER graphbuff-cli -u admin
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
    cargo build --release --locked --target "$(cat /target-triple)" -p graphbuff-server -p graphbuff-cli \
       --features graphbuff-server/tls,graphbuff-server/embed,graphbuff-cli/tls \
 && mkdir -p /out/bin /out/data /out/models \
 && cp "target/$(cat /target-triple)/release/graphbuff-server" \
       "target/$(cat /target-triple)/release/graphbuff-cli" /out/bin/

# glibc, libgcc and CA certificates only; runs as the unprivileged user `nonroot` (65532).
FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="graphbuff" \
      org.opencontainers.image.description="graphbuff graph database: Cypher over HTTP/JSON and Bolt" \
      org.opencontainers.image.source="https://github.com/JanVencl/graphbuff" \
      org.opencontainers.image.licenses="AGPL-3.0-only"
COPY --from=build /out/bin/ /usr/local/bin/
COPY packaging/docker/graphbuff.toml /etc/graphbuff/graphbuff.toml
COPY --from=build --chown=65532:65532 /out/data /data
COPY --from=build /out/models /models
USER nonroot
WORKDIR /data
VOLUME /data
# Databases and user accounts (the system database) in the /data volume; logs to standard
# error (`docker logs`). HTTP/JSON and Bolt listen on every interface of the container;
# which machines can reach them is decided by `docker run -p`.
ENV GRAPHBUFF_CONFIG=/etc/graphbuff/graphbuff.toml
EXPOSE 7475 7687
ENTRYPOINT ["graphbuff-server"]
