# The control side's image (ADR-t3990-3 decision 1): dagq's binary and the CA
# certificates, nothing else. It has no agent CLI and no git (ADR-t3987-1
# decision 3). The binary is static, so the image needs no libc and starts
# from an empty base. release.yml builds it from the root of the tag's
# checkout, once per architecture on a runner of that architecture.

FROM docker.io/library/rust:1-bookworm AS build
# CC_<target> because cc looks for a compiler named per target, which musl-tools
# may not have.
WORKDIR /source
COPY . .
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates musl-tools; \
    rustup toolchain install --no-self-update; \
    target="$(uname -m)-unknown-linux-musl"; \
    rustup target add "$target"; \
    export "CC_$(printf '%s' "$target" | tr - _)=musl-gcc"; \
    cargo build --release --locked --features postgres --bin dagq --target "$target"; \
    mkdir /out; \
    cp "target/$target/release/dagq" /out/dagq

FROM scratch
COPY --from=build /out/dagq /usr/local/bin/dagq
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/dagq"]
CMD ["service", "serve"]
