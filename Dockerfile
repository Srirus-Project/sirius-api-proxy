FROM rust:1.96-alpine AS chef
# cargo-chef compiles the dependency graph from a recipe, so source-only changes reuse that layer.
RUN apk add --no-cache musl-dev cmake && cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /app

FROM chef AS planner
# build.rs is part of the recipe so its build-dependencies are cooked too.
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY protocol ./protocol
COPY LICENSE* ./
# Declared after the cook so tag and dev builds share the dependency layer.
ARG VERSION=dev
RUN if [ "$VERSION" != "dev" ]; then grep -Fx "version = \"${VERSION#v}\"" Cargo.toml; fi
RUN cargo build --release --locked

FROM alpine:3.24
# git: master_git commits and pushes Master repositories by running the git executable.
# openssh-keygen: git signs and verifies commits with it when master_git.signing.format is ssh.
RUN apk add --no-cache ca-certificates tzdata git openssh-keygen && addgroup -S sirius && adduser -S -G sirius sirius
WORKDIR /app
COPY --from=builder /app/LICENSE* /usr/share/licenses/sirius-api-proxy/
COPY --from=builder /app/target/release/sirius-api-proxy /usr/local/bin/sirius-api-proxy
COPY --from=builder /app/protocol /app/protocol
RUN mkdir -p /app/master-data && chown sirius:sirius /app/master-data
ENV SIRIUS_CONFIG_PATH=/app/sirius-api-config.yaml
ARG VERSION=dev
LABEL org.opencontainers.image.version="${VERSION}"
USER sirius
ENTRYPOINT ["sirius-api-proxy"]
