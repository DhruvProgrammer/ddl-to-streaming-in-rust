# Build the website and serve it from a single, small binary.
#
#   docker compose up --build
#   open http://localhost:8787
#
# Stage 1 builds the Vite site, stage 2 builds the Rust server, stage 3 ships
# only the binary and the built assets.

# ---------------------------------------------------------------- frontend
FROM node:22-alpine AS web
WORKDIR /src
RUN npm --prefix frontend ci
COPY frontend/package.json frontend/package-lock.json ./frontend/
RUN npm --prefix frontend ci
COPY frontend ./frontend
RUN npm --prefix frontend run build

# ----------------------------------------------------------------- backend
FROM rust:1.83-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY backend/Cargo.toml backend/Cargo.lock* ./backend/
RUN mkdir -p backend/src \
    && echo 'fn main() {}' > backend/src/main.rs \
    && echo '' > backend/src/lib.rs \
    && (cd backend && cargo build --release --locked 2>/dev/null || cargo build --release)
COPY backend/src ./backend/src
COPY --from=web /src/frontend/dist ./frontend/dist
RUN cd backend \
    && touch src/main.rs src/lib.rs \
    && cargo build --release \
    && strip target/release/ddl-player

# ----------------------------------------------------------------- runtime
FROM alpine:3.20
RUN apk add --no-cache ca-certificates \
    && adduser -D -u 10001 ddl
WORKDIR /app
COPY --from=build /src/backend/target/release/ddl-player /app/ddl-player
COPY --from=web /src/frontend/dist /app/frontend/dist
USER ddl

ENV DDL_BIND=0.0.0.0:8787 \
    DDL_STATIC_DIR=/app/frontend/dist \
    DDL_LOG_JSON=1 \
    RUST_LOG=info

EXPOSE 8787
HEALTHCHECK --interval=15s --timeout=3s --start-period=3s \
    CMD wget -qO- http://127.0.0.1:8787/api/health || exit 1

ENTRYPOINT ["/app/ddl-player"]