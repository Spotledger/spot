module github.com/spotledger/sdk-templates/go

go 1.27.1

// The componentizer (componentize-go, Go 1.27.1+). `go mod tidy` resolves it and its bindings
// generator; `go tool componentize-go` then drives the build.
tool github.com/bytecodealliance/componentize-go
