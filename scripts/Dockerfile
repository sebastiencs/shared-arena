FROM ubuntu:20.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y \
    curl \
    build-essential \
    jq \
    cmake \
    valgrind \
    clang-10 \
    llvm-10-dev \
    && rm -rf /var/lib/apt/lists/* \
    && apt-get autoremove \
    && ln -s /usr/bin/clang-10 /usr/bin/clang \
    && ln -s /usr/bin/clang++-10 /usr/bin/clang++ \
    && ln -s /usr/bin/llvm-symbolizer-10 /usr/bin/llvm-symbolizer

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain none

RUN . ~/.cargo/env \
    && rustup set profile minimal \
    && rustup toolchain install nightly --component rust-src --allow-downgrade \
    && rustup override set nightly \

ENV PATH="/root/.cargo/bin:${PATH}"

CMD ["/bin/bash"]
