FROM rust:1.96-alpine AS builder
RUN apk add --no-cache musl-dev cmake
WORKDIR /app
COPY . .
ARG VERSION=dev
RUN if [ "$VERSION" != "dev" ]; then grep -Fx "version = \"${VERSION#v}\"" Cargo.toml; fi
RUN cargo build --release --locked

FROM alpine:3.24
RUN apk add --no-cache ca-certificates tzdata && addgroup -S sirius && adduser -S -G sirius sirius
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
