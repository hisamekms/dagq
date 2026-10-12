# The base image of dagq's container runtime (ADR-t2257-1 decision 2):
# only what the runtime needs. Claude Code, the Linux dagq client of the
# installed dagq's version and a non-root user. No language toolchain is
# kept: the client is built in a stage this image does not keep, and a
# project adds its own tools with the Containerfile dagq.toml names.
# dagq builds it in its podman machine with the build arguments below
# only: no secret, no host path and no environment go in.

# The Claude Code to install (domain::container_image::CLAUDE_CODE_VERSION).
ARG CLAUDE_CODE_VERSION
# The client: a release from its crates.io source (DAGQ_CLIENT_VERSION),
# or a development build from its commit (DAGQ_CLIENT_GIT, DAGQ_CLIENT_REV),
# built from a clean checkout so it names the same build
# (DAGQ_CLIENT_BUILD, which its --version must report).
ARG DAGQ_CLIENT_VERSION=
ARG DAGQ_CLIENT_GIT=
ARG DAGQ_CLIENT_REV=
ARG DAGQ_CLIENT_BUILD

FROM docker.io/library/rust:1-bookworm AS client
ARG DAGQ_CLIENT_VERSION
ARG DAGQ_CLIENT_GIT
ARG DAGQ_CLIENT_REV
ARG DAGQ_CLIENT_BUILD
RUN set -eu; \
    if [ -n "$DAGQ_CLIENT_REV" ]; then \
        git clone --quiet "$DAGQ_CLIENT_GIT" /source; \
        git -C /source checkout --quiet --detach "$DAGQ_CLIENT_REV"; \
        cargo install --locked --root /client --path /source; \
    else \
        cargo install --locked --root /client --version "=$DAGQ_CLIENT_VERSION" dagq; \
    fi; \
    version="$(/client/bin/dagq --version)"; \
    if [ "$version" != "dagq $DAGQ_CLIENT_BUILD" ]; then \
        echo "the client reports $version, not dagq $DAGQ_CLIENT_BUILD" >&2; \
        exit 1; \
    fi

FROM docker.io/library/node:22-bookworm-slim
ARG CLAUDE_CODE_VERSION
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates git; \
    rm -rf /var/lib/apt/lists/*; \
    npm install --global "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"; \
    npm cache clean --force; \
    groupadd --gid 10001 dagq; \
    useradd --uid 10001 --gid dagq --create-home --shell /bin/bash dagq
COPY --from=client /client/bin/dagq /usr/local/bin/dagq
# The runtime chooses the Claude Code; it does not update itself.
ENV DISABLE_AUTOUPDATER=1
USER dagq
WORKDIR /home/dagq
