# syntax=docker/dockerfile:1

FROM rust:slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc \
        g++ \
        libc6-dev \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release -p anda_brain

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tzdata \
        openssl \
    && update-ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user. The fixed UID/GID (10001) keeps ownership of
# host-mounted volumes (e.g. ./db) predictable across environments.
RUN groupadd --system --gid 10001 anda \
    && useradd --system --uid 10001 --gid 10001 \
        --home-dir /app --shell /usr/sbin/nologin anda

WORKDIR /app

# The service writes data (e.g. db/) under the working directory.
RUN chown anda:anda /app

COPY --from=builder /build/target/release/anda_brain /usr/local/bin/anda_brain

ENV LISTEN_ADDR=0.0.0.0:8042

EXPOSE 8042

USER anda

ENTRYPOINT ["/usr/local/bin/anda_brain"]
CMD []