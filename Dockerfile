ARG EDS_IMAGE_PREFIX=eds

FROM localhost/${EDS_IMAGE_PREFIX}-certificates AS certificates

# Stage 1: Build
FROM rust:slim-trixie AS builder

ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    CARGO_HTTP_CAINFO=/etc/ssl/certs/ca-certificates.crt

COPY --from=certificates /opt/proxy/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=certificates /opt/proxy/certs/share/ /usr/local/share/ca-certificates/

WORKDIR /build

# Pre-build dependencies (cached unless Cargo.toml/Cargo.lock change)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

# Build the real binary
COPY src/ src/
COPY migrations/ migrations/
COPY assets/ assets/
COPY i18n/ i18n/
RUN touch src/main.rs && cargo build --release

# Stage 2: Runtime
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=certificates /opt/proxy/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=certificates /opt/proxy/certs/share/ /usr/local/share/ca-certificates/

RUN useradd -r -s /bin/false -m -d /var/lib/calrs calrs

COPY --from=builder /build/target/release/calrs /usr/local/bin/calrs
COPY templates/ /opt/calrs/templates/

WORKDIR /opt/calrs
USER calrs

ENV CALRS_DATA_DIR=/var/lib/calrs \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 3000

ENTRYPOINT ["calrs"]
CMD ["serve", "--host", "0.0.0.0", "--port", "3000"]
