FROM rust:alpine AS build

WORKDIR /usr/src
RUN apk add musl-dev ca-certificates
RUN update-ca-certificates

RUN cargo install cargo-bloat
RUN USER=root cargo new jungfrau-ctrl-ioc
WORKDIR /usr/src/jungfrau-ctrl-ioc

COPY Cargo.toml Cargo.lock ./
COPY epicars epicars
COPY src ./src

RUN cargo build
RUN cargo bloat --crates
RUN cargo install --root=/opt --path . --debug

FROM gcr.io/distroless/cc-debian12
COPY --from=build /opt/bin/jungfrau-ctrl-ioc /
ENTRYPOINT ["/jungfrau-ctrl-ioc"]


