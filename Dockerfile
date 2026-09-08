# syntax=docker/dockerfile:1

# Stage 1: the Vue bundle.
FROM node:24-bookworm-slim AS frontend
WORKDIR /app/frontend
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

# Stage 2: the binary, with the bundle embedded by rust-embed.
FROM rust:1.96-bookworm AS backend
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
COPY --from=frontend /app/frontend/dist ./frontend/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && cp target/release/gmcp /usr/local/bin/gmcp

# Stage 3: runtime.
#
#   * poppler-utils is `pdftotext`, which the text extractor shells out to for
#     PDFs. Its absence is not fatal — `/api/health` and the tool error both
#     report it — but a PDF attachment is unreadable without it.
#   * sqlite3 is for a shell on the one database file: backups, `.schema`, the
#     occasional look at the audit log without the portal.
#   * curl is for the HEALTHCHECK below and nothing else.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl poppler-utils sqlite3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=backend /usr/local/bin/gmcp /usr/local/bin/gmcp
# Non-root, at a fixed uid: the database is a bind mount from the host, so the
# uid is part of the deployment contract. docs/deploy.md chowns `data/` to it.
RUN groupadd --system --gid 10001 gmcp \
    && useradd --system --uid 10001 --gid 10001 --create-home gmcp \
    && install -d -o gmcp -g gmcp /data
USER gmcp
# The container's own defaults. `GMCP_DATABASE` points at the volume, so a
# `.env` that forgets it still stores the database where the mount is.
ENV GMCP_BIND=0.0.0.0:8000 \
    GMCP_DATABASE=/data/gmcp.db
VOLUME ["/data"]
EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:8000/api/health || exit 1
ENTRYPOINT ["gmcp"]
CMD ["serve"]
