# spot-bpm-documents

Document nodes and the Case file for the Business Process engine (ADR-49, ADR-080).

## Publishing a version that acts as a service principal (WP-BPM-04.T4)

A Document node that names a `principal` acts as a registered service principal. The publish
path must call, once per version and before it registers the version:

```rust
let violations = spot_bpm_documents::validate_process_version(
    &guard,        // PublishGuard::new(IamServicePrincipals, IamPublishPermission)
    &tenant,       // SiteId
    &author,       // ProcessAuthor
    &compiled.dag, // spot_dag::CompiledDag of the compiled version
);
if !violations.is_empty() { /* refuse the publish, report every violation */ }
```

`IamServicePrincipals` reads the tenant's service-principal entries (and their grant lists)
through the injected `ServicePrincipalDirectory`; `IamPublishPermission` asks the tenant's
`PermissionEngine` whether the author holds `process.publish_service_principal` (asked of the
engine as `process_publish_service_principal`, because its named-permission grammar has no dot).
Every failure answers "no".
