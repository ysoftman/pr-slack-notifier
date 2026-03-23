FROM rust:1.85-slim AS builder
WORKDIR /app

# 의존성 캐싱: Cargo.toml/Cargo.lock만 복사 후 dummy build
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release --locked && rm -rf src

# 실제 소스 빌드
COPY src/ src/
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -r -s /usr/sbin/nologin notifier
COPY --from=builder /app/target/release/pr-slack-notifier /usr/local/bin/
USER notifier
ENTRYPOINT ["pr-slack-notifier"]
