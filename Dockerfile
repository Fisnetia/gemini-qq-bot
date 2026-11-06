# syntax=docker/dockerfile:1.7
#
# Gemini QQ bot: Rust service + Mihomo + Agy CLI in one container.
#
# Everything is pinned. Mihomo comes from an official MetaCubeX release and its
# archive hash is verified during the build. Agy has **no reproducible public
# install channel** we could confirm, so it is supplied as a build artifact whose
# SHA-256 the builder must state explicitly; without it the image is built without
# the CLI and the service reports the missing contract as unverified rather than
# pretending to be production ready.

# --------------------------------------------------------------------- vendored pins
# Pinned for reproducibility; bump deliberately, never implicitly.
ARG RUST_IMAGE=rust:1.99-bookworm
ARG DEBIAN_IMAGE=debian:bookworm-slim
ARG MIHOMO_VERSION=v1.19.30
# sha256 of mihomo-linux-amd64-compatible-${MIHOMO_VERSION}.gz as published by
# MetaCubeX. Verified locally: the archive expands to a binary reporting
# "Mihomo Meta v1.19.30" with the with_gvisor tag.
ARG MIHOMO_ARCHIVE_SHA256=db214c7a2517e63c150d123178d16d102e03a241ccdae4e5e07ffbe9cf56c6f9
ARG MIHOMO_URL=https://github.com/MetaCubeX/mihomo/releases/download/${MIHOMO_VERSION}/mihomo-linux-amd64-compatible-${MIHOMO_VERSION}.gz

# ---------------------------------------------------------------- builder: rust bin
FROM ${RUST_IMAGE} AS rusty

WORKDIR /build

# Build the binary.
#
# CRATES_INDEX lets the builder point at a mirror. It must be a full sparse URL with a
# scheme (`http://host/path`): the template below prepends `sparse+`, so a bare mirror
# name like `USTC` becomes `sparse+USTC` and cargo rejects it with
# `invalid url `sparse+USTC`: relative URL without a base`. `--locked` is kept so the
# pinned versions in Cargo.lock are the ones compiled, mirror or not.
#
# There is deliberately no separate "cache the dependencies with an empty src/" step:
# that trick leaves a stub `libgemini_qq_bot` rlib in `target/` that the real build then
# reuses, and the compile fails with `could not find \`config\` in \`gemini_qq_bot\``.
# A full dependency build is well under a minute, so the complexity buys nothing.
ARG CRATES_INDEX=""
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN if [ -n "${CRATES_INDEX}" ]; then \
         mkdir -p .cargo \
         && printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "sparse+%s"\n' "${CRATES_INDEX}" > .cargo/config.toml; \
    fi \
    && cargo build --release --locked \
    && strip target/release/gemini-qq-bot

# ------------------------------------------------------------ builder: mihomo binary
FROM ${DEBIAN_IMAGE} AS mihomo-fetch

ARG MIHOMO_URL
ARG MIHOMO_ARCHIVE_SHA256
# Debian archive mirror and a proxy for the GitHub download. Containers on the default
# bridge cannot reach deb.debian.org or github.com directly in this environment, so
# both are parameterised rather than hard-wired; the defaults are the canonical ones.
ARG APT_MIRROR=""
ARG DOWNLOAD_PROXY=""

RUN set -eu; \
    if [ -n "${APT_MIRROR}" ]; then \
        sed -i "s|http://deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources 2>/dev/null \
        || sed -i "s|http://deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl; \
    rm -rf /var/lib/apt/lists/*; \
    curl -fsSL ${DOWNLOAD_PROXY:+-x "${DOWNLOAD_PROXY}"} -o /tmp/mihomo.gz "${MIHOMO_URL}"; \
    echo "${MIHOMO_ARCHIVE_SHA256}  /tmp/mihomo.gz" | sha256sum -c -; \
    gunzip -c /tmp/mihomo.gz > /out-mihomo; \
    chmod 0755 /out-mihomo; \
    /out-mihomo -v

# --------------------------------------------------------------------- builder: agy
#
# `agy` 1.4.9 is distributed as a bare self-contained binary whose SHA-256 was
# observed locally but for which no reproducible public release endpoint could be
# confirmed. Rather than shipping a fake download step, the build takes the binary
# from the build context and refuses to continue if the operator did not pin a hash.
#
# Build with:
#   docker build --build-arg AGY_SHA256=<sha256 of ./vendor/agy> ...
#
# The default leaves both empty, producing an image without the CLI; health then
# reports the Agy contract as unverified.
FROM ${DEBIAN_IMAGE} AS agy-fetch

ARG AGY_SHA256=""
COPY vendor/ /vendor/

RUN set -eu; \
    mkdir -p /out; \
    if [ -n "${AGY_SHA256}" ]; then \
        test -f /vendor/agy || { echo "AGY_SHA256 set but vendor/agy missing" >&2; exit 1; }; \
        echo "${AGY_SHA256}  /vendor/agy" | sha256sum -c -; \
        cp /vendor/agy /out/agy; \
        chmod 0755 /out/agy; \
    else \
        echo "AGY_SHA256 empty: building without the Agy CLI" >&2; \
    fi

# ------------------------------------------------------------------------ runtime
FROM ${DEBIAN_IMAGE} AS runtime

ARG MIHOMO_VERSION
ARG APT_MIRROR=""

RUN set -eu; \
    if [ -n "${APT_MIRROR}" ]; then \
        sed -i "s|http://deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources 2>/dev/null \
        || sed -i "s|http://deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates tzdata; \
    rm -rf /var/lib/apt/lists/*; \
    groupadd --gid 10001 bot; \
    useradd --uid 10001 --gid bot --home-dir /data/agy-home --no-create-home \
            --shell /usr/sbin/nologin bot; \
    install -d -o bot -g bot -m 0700 /data /data/agy-home /run/secrets; \
    install -d -o root -g root -m 0755 /opt/mihomo /opt/agy

COPY --from=mihomo-fetch /out-mihomo /opt/mihomo/mihomo
COPY --from=agy-fetch /out/ /opt/agy/
COPY --from=rusty /build/target/release/gemini-qq-bot /usr/local/bin/gemini-qq-bot

# The image is read-only at runtime; only these two paths are writable mounts.
VOLUME ["/data"]
# /tmp is a tmpfs supplied by the runtime (image downloads are staged and deleted here).

ENV RUST_LOG=info \
    GEMINI_BOT_CONFIG=/etc/gemini-bot/config.toml \
    AGY_BIN=/opt/agy/agy \
    AGY_HOME=/data/agy-home \
    MIHOMO_BIN=/opt/mihomo/mihomo \
    MIHOMO_CONFIG_PATH=/etc/mihomo/config.yaml \
    GEMINI_DB_PATH=/data/bot.sqlite3 \
    AUX_API_KEY_FILE=/run/secrets/aux_api_key \
    ADMIN_BIND=0.0.0.0:8080

USER 10001:10001

EXPOSE 8080

# The readiness endpoint is behind the internal bearer token, so the health check
# only asserts the listener answers at all; deeper checks are in /internal/v1/status.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/gemini-qq-bot", "--health-probe"]

ENTRYPOINT ["/usr/local/bin/gemini-qq-bot"]
