# syntax=docker/dockerfile:1.4
FROM rust:alpine3.20 as build-environment

ARG TARGETARCH
WORKDIR /opt

RUN apk add lld build-base linux-headers pkgconf openssl-dev

RUN echo "export RUSTFLAGS='-Ctarget-feature=-crt-static'" >> $HOME/.profile
# Mac M1 workaround
RUN [[ "$TARGETARCH" = "arm64" ]] && echo "export CFLAGS=-mno-outline-atomics" >> $HOME/.profile || true

# Install nightly and set as default toolchain
RUN rustup update nightly && rustup default nightly

WORKDIR /opt/challenger
COPY . .

RUN source $HOME/.profile \
    && cargo build --release \
    && mkdir out \
    && mv target/release/challenger out/challenger \
    && strip out/challenger;

# Runner image
FROM alpine:3.20 as challenger-client

RUN apk add --no-cache linux-headers gcompat libgcc

COPY --from=build-environment /opt/challenger/out/challenger /usr/local/bin/challenger

RUN adduser -Du 1000 challenger

ENTRYPOINT ["challenger"]

LABEL org.label-schema.build-date=$BUILD_DATE \
    org.label-schema.name="Challenger" \
    org.label-schema.description="Challenger" \
    org.label-schema.version=$VERSION \
    org.label-schema.schema-version="1.0"
