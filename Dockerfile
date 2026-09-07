# dendro 服务器镜像（多阶段构建）
FROM rust:1.85-slim AS build
WORKDIR /src
RUN apt-get update && apt-get install -y pkg-config protobuf-compiler && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
RUN cargo build --release -p dendro-server || cargo build --release -p dendro-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/dendro /usr/local/bin/dendro
EXPOSE 5432 3306
ENTRYPOINT ["dendro"]
CMD ["serve", "--data", "/data"]
