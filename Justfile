set shell := ["bash", "+u", "-c"]

alias cov := coverage

test:
    if ! cargo test --features=all; then \
        just test-clean; \
        exit 1; \
    fi

test-clean:
    rm -f /tmp/git-credential-keepassxc.test_*.json
    [[ -z "$TMPDIR" ]] || rm -f "$TMPDIR"/git-credential-keepassxc.test_*.json

check:
    for feature in default notification encryption yubikey all; do \
        cargo check --features=$feature; \
    done

build:
    for feature in default notification encryption yubikey all; do \
        cargo build --release --features=$feature; \
    done

build-win:
    env PKG_CONFIG_ALLOW_CROSS=1 cargo build --features=all --release --target=x86_64-pc-windows-gnu

coverage:
    env CARGO_INCREMENTAL=0 RUSTFLAGS="-Zprofile -Ccodegen-units=1 -Copt-level=0 -Clink-dead-code -Coverflow-checks=off -Zpanic_abort_tests -Cpanic=abort" \
        RUSTDOCFLAGS="-Cpanic=abort" cargo build --features=all
    env CARGO_INCREMENTAL=0 RUSTFLAGS="-Zprofile -Ccodegen-units=1 -Copt-level=0 -Clink-dead-code -Coverflow-checks=off -Zpanic_abort_tests -Cpanic=abort" \
        RUSTDOCFLAGS="-Cpanic=abort" cargo test --features=all
    just test-clean
    grcov ./target/debug/ -s . -t html --llvm --branch --ignore-not-existing -o ./target/debug/coverage/
    if command -v xdg-open 2>&1 >/dev/null; then \
        xdg-open ./target/debug/coverage/index.html; \
    elif command -v open 2>&1 >/dev/null; then \
        open ./target/debug/coverage/index.html; \
    fi
