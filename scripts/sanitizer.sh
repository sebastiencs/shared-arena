#!/usr/bin/env bash

set -xe

TARGET=$(rustc -Z unstable-options --print target-spec-json | jq -r .\"llvm-target\")

if [ "$2" == "release" ]; then
    RELEASE_FLAG="--release"
fi

case "$1" in
    address)
        export CC="clang"
        export CXX="clang++"
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
    coverage)
        export CARGO_INCREMENTAL="0"
        export RUSTFLAGS="-Zprofile -Ccodegen-units=1 -Copt-level=0 -Clink-dead-code -Coverflow-checks=off -Zpanic_abort_tests -Cpanic=abort"
        export RUSTDOCFLAGS="-Cpanic=abort"

        cargo build --tests
        cargo test --no-fail-fast
        # cargo test --all-features --no-fail-fast

        grcov ./target/debug/ -s . -t lcov --llvm --branch --ignore-not-existing -o ./coverage.info --ignore "*rust/library*" --ignore "*registry*" --excl-line "grcov_ignore|assert"
        # grcov ./target/debug/ -s src/ -t lcov --llvm --branch --ignore-not-existing -o ./coverage.info --ignore "*registry*" --token ${{ secrets.CODECOV_TOKEN }} --excl-line "grcov_ignore|assert"

        # grcov ./target/debug/ -s src/ -t html --llvm --branch --ignore-not-existing -o ./target/debug/coverage --ignore "*rust/src*" --ignore "*registry*" --excl-line "grcov_ignore|assert"

        # grcov ./target/debug/ -s src/ -t lcov --llvm --branch --ignore-not-existing -o ./coverage.info --ignore "*rust/src*" --ignore "*registry*" --excl-line "grcov_ignore|assert"
        # grcov ./target/debug/ -s src/ -t coveralls+ --llvm --branch --ignore-not-existing -o ./coverage.json --ignore "*registry*" --token ${{ secrets.CODECOV_TOKEN }} --excl-line "grcov_ignore|assert"
        exit
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
