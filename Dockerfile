# dendro 服务器镜像（多阶段构建）
FROM rust:1.85-slim AS build
WORKDIR /src
RUN apt-get update && apt-get install -y pkg-config protobuf-compiler && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
RUN cargo build --release -p dendro-server || cargo build --release -p dendro-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 dendro && mkdir -p /data /tmp/dendro-cache && chown -R dendro /data /tmp/dendro-cache
COPY --from=build /src/target/release/dendro /usr/local/bin/dendro
USER dendro
EXPOSE 5432 3306 6380 9469
HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
  CMD curl -fsS http://127.0.0.1:9469/readyz || exit 1
ENTRYPOINT ["dendro"]
CMD ["serve", "--data", "/data", "--host", "0.0.0.0"]
