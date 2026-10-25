# Static musl build of overleaf-mcp in an empty image (no shell, no libc; only the CA bundle).
FROM rust:1.99-alpine AS build
RUN apk add --no-cache musl-dev ca-certificates
WORKDIR /src
# dependencies first, so source edits do not rebuild them
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /src/target/release/overleaf-mcp /overleaf-mcp
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/overleaf-mcp"]
