ARG USER=developer
ARG GROUP=developers
ARG UID=10000
ARG GID=2001

FROM rust:1.57 AS solana-build
COPY . /solana
RUN apt-get update && \
    apt-get install -y --no-install-recommends libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang && \
    rm -rf /var/lib/apt/lists/*
ENV RUSTFLAGS="-C target-feature=+avx,+avx2 ${RUSTFLAGS}" \
    TOML_CONFIG=/solana/config.toml \
    PATH="/solana/target/release:${PATH}" \
    NDEBUG=1
WORKDIR /solana
RUN \
    rustup component add rustfmt && \
    rustup component add clippy 
RUN rustup --version; \
    cargo --version; \
    rustc --version; \
    cargo build --release --all
RUN ./multinode-demo/setup.sh
# IMPORTANT! To run inside docker swarm you shouldn't check ports because it leads to fail.
# The next row will set --no-port-check flag when solana-validator will be called
RUN sed -i '/^default_arg\ \-\-require\-tower=*/adefault_arg --no-port-check' multinode-demo/validator.sh && \
    cp -R config ./config_default && \
    rm -rf target/release/deps
# rm -rf target/release/build \
    #       target/release/deps \
     #      target/release/.fingerprint \
      #     target/release/*.d  \
       #    target/release/libsolana_*  \
        #   target/release/*.rlib  \
         #  target/release/*.so \
         #  target/release/cargo-build-bpf \
         #  target/release/solana-accounts-bench \
         #  target/release/solana-banking-bench \
         #  target/release/solana-bench-exchange \
         #  target/release/solana-bench-streamer \
         #  target/release/solana-bench-tps \
         #  target/release/solana-csv-to-validator-infos \
         #  target/release/solana-dos \
         #  target/release/solana-install \
         #  target/release/solana-install-init \
         #  target/release/solana-ip-address \
         #  target/release/solana-ip-address-server \
         #  target/release/solana-net-shaper \
         #  target/release/solana-poh-bench \
         #  target/release/solana-ramp-tps \
         #  target/release/solana-stake-accounts \
         #  target/release/solana-stake-monitor \
         #  target/release/solana-stake-o-matic \
         #  target/release/solana-sys-tuner \
          # target/release/solana-tokens \
          # target/release/solana-upload-perf \
         #  target/release/solana-vote-signer \
        #   target/release/solana-watchtower

FROM ubuntu:20.04
ARG USER
ARG GROUP
ARG UID
ARG GID
ENV PATH="/solana/target/release/:${PATH}" \
    TOML_CONFIG=/solana/config.toml \
    USE_INSTALL=1
RUN groupadd -g $GID $GROUP && \
    useradd -p '*' -m -u $UID -g $GROUP -s /bin/bash $USER && \
    apt-get update && \
    apt-get install -y --no-install-recommends libssl-dev sudo && \
    echo "developer ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers && \
    rm -rf /var/lib/apt/lists/*
COPY --from=solana-build --chown=${UID}:${GID} /solana /solana
USER developer
WORKDIR /solana
