FROM node:22-bookworm-slim AS frontend
WORKDIR /app/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

FROM rust:1-bookworm AS backend
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends libsqlite3-dev pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY server ./server
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg libsqlite3-0 && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home plate-review \
    && mkdir -p /app/state /app/web/dist /data/exports \
    && chown -R plate-review:plate-review /app /data/exports
COPY --from=backend /app/target/release/plate-review /usr/local/bin/plate-review
COPY --from=frontend /app/web/dist /app/web/dist
ENV PLATE_REVIEW_BIND=0.0.0.0:8080 \
    PLATE_REVIEW_STATE_DIR=/app/state \
    PLATE_REVIEW_EXPORT_DIR=/data/exports \
    PLATE_REVIEW_WEB_DIR=/app/web/dist
USER plate-review
EXPOSE 8080
VOLUME ["/app/state"]
ENTRYPOINT ["plate-review"]
CMD ["serve"]
