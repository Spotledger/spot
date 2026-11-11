# The Python sample-app guest (WP-STR-07.T1). `make build` produces out/component.wasm.
.PHONY: build bindings check clean

build:
	./build.sh

bindings:
	componentize-py -d wit -w test-world bindings guest_bindings

check: build
	wasm-tools component wit out/component.wasm

clean:
	rm -rf out guest_bindings
