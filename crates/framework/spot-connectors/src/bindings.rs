//! The generated bindings for the `connector` world.

wasmtime::component::bindgen!({
    path: "wit/connector.wit",
    world: "connector",
    imports: { default: async | trappable },
    exports: { default: async },
});
