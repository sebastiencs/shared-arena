#!/usr/bin/env bash

set -xe

TARGET=$(rustc -Z unstable-options --print target-spec-json | jq -r .\"llvm-target\")

case "$1" in
    address)
        export CFLAGS="-fsanitize=address"
        export CXXFLAGS="-fsanitize=address"
        export RUSTFLAGS="-Zsanitizer=address"
        export RUSTDOCFLAGS="-Zsanitizer=address"

        CMD="cargo test -Z build-std --target $TARGET"
        ;;
    leak)
        export RUSTFLAGS="-Zsanitizer=leak"
        export RUSTDOCFLAGS="-Zsanitizer=leak"

        CMD="cargo test --target $TARGET"
        ;;
    memory)
        export CFLAGS="-fsanitize=memory -fsanitize-memory-track-origins"
        export CXXFLAGS="-fsanitize=memory -fsanitize-memory-track-origins"
        export RUSTFLAGS="-Zsanitizer=memory -Zsanitizer-memory-track-origins"
        export RUSTDOCFLAGS="-Zsanitizer=memory -Zsanitizer-memory-track-origins"

        CMD="cargo test -Z build-std --target $TARGET"
        ;;
    *)
        echo -e "Available commands: address, leak, memory\n"
        echo -e "Example:\n\t$0 leak"
        exit 1
esac

for i in {1..100}
do
    $CMD
done
