# The Go sample-app guest (WP-STR-07.T1). `make build` produces out/component.wasm.
.PHONY: build bindings check clean

build:
	./build.sh

bindings:
	componentize-go --world test-world bindings --format
	go mod tidy

check: build
	wasm-tools component wit out/component.wasm

clean:
	rm -rf out main.wasm
