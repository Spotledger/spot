# The reference guest (WP-STR-07.T1). `make build` produces out/component.wasm.
.PHONY: build check clean

build:
	./build.sh

check: build
	wasm-tools component wit out/component.wasm

clean:
	rm -rf out target
