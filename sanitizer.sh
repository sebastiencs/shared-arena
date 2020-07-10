#!/usr/bin/env bash

set -xe

TARGET=$(rustc -Z unstable-options --print target-spec-json | jq -r .\"llvm-target\")

if [ "$2" == "release" ]; then
    RELEASE_FLAG="--release"
fi

case "$1" in
    address)
        export CFLAGS="-fsanitize=address"
        export CXXFLAGS="-fsanitize=address"
        export RUSTFLAGS="-Zsanitizer=address"
        export RUSTDOCFLAGS="-Zsanitizer=address"

        CMD="cargo test -Z build-std --target $TARGET $RELEASE_FLAG"
        ;;
    leak)
        export RUSTFLAGS="-Zsanitizer=leak"
        export RUSTDOCFLAGS="-Zsanitizer=leak"

        CMD="cargo test --target $TARGET $RELEASE_FLAG"
        ;;
    memory)
        export CC="clang"
        export CXX="clang++"
        export CFLAGS="-fsanitize=memory -fsanitize-memory-track-origins"
        export CXXFLAGS="-fsanitize=memory -fsanitize-memory-track-origins"
        export RUSTFLAGS="-Zsanitizer=memory -Zsanitizer-memory-track-origins"
        export RUSTDOCFLAGS="-Zsanitizer=memory -Zsanitizer-memory-track-origins"

        CMD="cargo test -Z build-std --target $TARGET $RELEASE_FLAG"
        ;;
    valgrind)
        cargo build --tests $RELEASE_FLAG

        EXECUTABLE=$(find target/${2:-debug}/deps/shared_arena* -type f -executable -print)
        CMD="valgrind --error-exitcode=1 $EXECUTABLE"
        ;;
    *)
        echo -e "Available commands: address, leak, memory, valgrind\n"
        echo -e "Example:\n\t$0 leak"
        exit 1
esac

for i in {1..100}
do
    echo "$i/100"
    $CMD
done
