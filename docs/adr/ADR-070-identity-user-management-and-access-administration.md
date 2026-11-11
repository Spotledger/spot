# ADR-070: Identity, user management and access administration

- **Status:** Proposed by the lead, 6 Oct 2026, on the architecture owner's direction. All open questions answered by the architecture owner on 6 Oct 2026 (below); awaits the formal approval of the ADR.
- **Date:** 6 Oct 2026
- **Layer:** Platform (new work packages WP-IAM-01 to WP-IAM-10 and WP-SEC-10; one additive amendment to the events contract, see "Consequences")
- **Builds on:** ADR-34 and ADR-058 (tenancy), ADR-51 (lifecycle phases and their permissions), ADR-053 and ADR-055 (storage and native API contracts), ADR-056 and ADR-061 (events and journal), ADR-062 to ADR-067 (sync), ADR-068 (`permlevel`; hidden is not security), WP-PLT-02, WP-PLT-03, WP-PLT-04, WP-SEC-02, WP-SEC-04, WP-AI-06, WP-ADM-11
- **Raised by:** the architecture owner, 6 Oct 2026: "user management and user permissions: do we have an ADR?" There was none. The primitives (`spot-auth`, the `spot-permissions` decision core) are briefed, but nothing says what a user is, where roles come from, how access is granted, reviewed and revoked, or how people sign in through a company identity provider.

## Context

The platform has a permission evaluator in design (WP-PLT-03: roles, per-DocType rights by permission level, user permissions, shares, owner rules) and credential primitives (WP-PLT-02). Both are pure libraries. A customer cannot run a business on them alone. Six questions are open:

1. **What is a user?** A person, a service, an agent, a customer's portal login, the platform itself.
2. **Who owns identity?** The tenant database or a platform directory (ADR-058 makes the tenant the unit of residency).
3. **Where do roles come from, and how are they changed safely?** Hand-built role grids do not survive a 500-user company or a release upgrade.
4. **How is data scope set per user?** "This accountant sees only Company B" is the daily request of every multi-entity customer.
5. **How do users arrive and leave?** Invitations, company SSO, automated provisioning and, above all, deprovisioning.
6. **How are risky grants controlled and proven to an auditor?** Segregation of duties, access reviews, emergency access, and a full audit trail.

### What the tier-1 ERPs do

Researched on 6 Oct 2026 from the vendors' public documentation (concepts only; the project coding rules applies: no ERP code or definitions are copied). Sources are listed at the end.

| | SAP S/4HANA | Dynamics 365 Finance | Oracle Fusion | NetSuite | Workday | Odoo |
| --- | --- | --- | --- | --- | --- | --- |
| Unit of access | Role (PFCG) made of authorization objects with organisational field values | Role → Duty → Privilege → Permission | Job role → Duty role → Privilege; Abstract roles | Role with permission list and level (None/View/Create/Edit/Full) | Security group → Domain/Business-process policy | Group → model ACL + record rules |
| Data scope | Org-level field values in the role (company code, plant) | Data security policies by organisation; period-based access (current/past/future) | **Data role** = job role limited to a dimension; data security policies on instance sets | Subsidiary / department / location restrictions on the role | **Constrained** (by org) vs **unconstrained** groups | Record rules; company switcher, but groups are global across companies |
| Reference roles | Delivered, copied before use | Sample roles, copied; guidance to re-audit after each service update | **Security reference implementation**, delivered read-only, extended by copy | Standard roles, copied | Delivered groups; delivered groups carry SoD conflicts | Delivered groups |
| Role assignment | Manual, or via GRC workflow | **Automatic rules** (HR position → roles); Entra ID group sync; role hierarchy | Role provisioning rules from HR data | Manual per employee | Role-based groups assigned to **positions**; user-based groups for exceptions | Manual |
| Identity and SSO | SAML/OIDC to IdP; central user store optional | Entra ID only | Identity domain / IdP federation | SAML SSO is a permission; the Administrator role **cannot** use SSO (lock-out protection); SSO roles cannot be switched to non-SSO roles | SAML/OIDC | OAuth/LDAP/SAML modules |
| Governance | GRC Access Control: SoD risk analysis at request time, **firefighter** emergency access with logged session reviewed by a controller, periodic **user access review** | Segregated duties by design; SoD conflict reports | Role-conflict analysis (Risk Management) | Audit trail on role changes | SoD built around business-process security | Audit through logs |

### What the tier-1 pattern gives us

- **A layer between the role and the permission.** SAP, Dynamics and Oracle all insert a named, reviewable unit (SAP authorization object bundles, Dynamics *duty*, Oracle *duty role*) because a role of 600 raw rights cannot be understood, tested or upgraded. Dynamics says outright: assign duties, not privileges.
- **Data scope is part of the grant, not a separate chore.** Oracle's *data role*, Workday's *constrained* groups and SAP's org-level values all say the same thing: one job (Accountant) plus a scope (Company B). Odoo's global groups are the counter-example: the same Accounting Manager rights follow the user into every company.
- **Assignment is driven by business data.** Dynamics (position → roles) and Workday (groups assigned to positions) remove the admin from the loop when someone changes jobs. Without it, access only grows.
- **Delivered roles are read-only reference; customers copy.** Vendors keep re-delivering them with each release (Oracle's reference implementation, Dynamics' post-update steps). Editing the delivered role directly makes upgrades unsafe.
- **Separation of duties is checked when access is granted**, with a periodic review afterwards (SAP GRC). Workday's lesson: a delivered group can contain a conflict inside itself.
- **Emergency access is a separate, logged, time-boxed path** (SAP firefighter: owner, controller, session log reviewed afterwards), not a shared super-user.
- **Never lock yourself out** (NetSuite): an administrator who is not tied to the customer's IdP always exists.
- **Deprovisioning is the security control.** The SCIM convention (RFC 7643, RFC 7644) is `active=false`, not delete: the record stays for audit and rehire, and sessions and tokens die at once.

## Options

**A. Ship PLT-02 and PLT-03 as briefed and let apps invent the rest.** Each app would build its own user screens and role grids. Rejected: this is how permissions end up in ten shapes, and the ERP needs one model for SoD, review and audit.

**B. A global identity service above tenants** (one account, many tenants). Convenient for consultants and group accountants. Rejected as the primary model: it contradicts ADR-034/ADR-058 (the tenant is the unit of placement and residency, and may be on a laptop or a branch node), makes sign-in depend on a control plane, and moves personal data out of its tenant's region.

**C. Tenant-owned identity and authorisation, with optional federation outward.** Everything the permission engine needs lives in the tenant database (and so syncs, backs up and is residency-bound with it). Sign-in can be delegated to a customer IdP; a control-plane "identity link" can map one person to several tenants for convenience without owning their access.

## Decision

Adopt C, with the rules below.

### 1. The principal model

1. **`User`** is a framework DocType in the tenant, with a stable record id (the `Actor::User { id }` of the events contract). It carries a **`user_type`**:
   - `internal`: staff with desk access;
   - `portal`: a customer, supplier or other outside party, reaching only its own records through party scoping (rule 6);
   - `service`: a non-human account that owns API keys and integration credentials;
   - `agent`: an AI agent identity with a tier (WP-AI-06).

   `System` actors (jobs, sync apply, migrations) stay as in the events contract and are not users.
2. A user is **never deleted** once it has acted. `enabled=false` deactivates; the id stays in audit and events. Anonymisation (privacy request) overwrites personal fields in place and keeps the id.
3. **One account per person per tenant.** A control-plane identity link (rule 14) may relate accounts across tenants but carries no rights.
4. The sign-in name is an e-mail address or an explicit username; the IdP subject (`iss` + `sub`) is a separate linked-identity record, so changing a work e-mail does not orphan the account.

### 2. Roles: reference packs, copies, duties

1. **Duty** is a named, versioned bundle of grants (DocType rights by permlevel, report access, page access, whitelisted methods) that matches a part of a business process ("Maintain bank transactions", "Approve purchase orders"). Duties are defined by apps in their packs and are the unit SoD rules reason about.
2. **Role** is a named set of duties plus optional direct grants. Roles may inherit from other roles (acyclic, depth limit 8, evaluated at snapshot build). A role records **`origin`**: `reference` (delivered by an app pack, read-only, replaced on upgrade) or `custom` (a tenant's own).
3. **Customising a reference role means copying it** into a custom role that remembers its source and version. On upgrade the system shows the difference between the new reference and the copy; it never rewrites the copy. This is the one rule that keeps releases safe.
4. Roles and duties are **metadata records in the tenant**, loaded from packs and editable through the normal document lifecycle, so they are versioned, audited and synced like any other metadata (ADR-062 to ADR-067). The permission engine consumes an immutable **Snapshot** built from them (WP-PLT-03 T1). A change publishes a new snapshot atomically; in-flight requests finish under the snapshot they started with.
5. The engine's existing evaluation rules stand (WP-PLT-03 T1): rights are the union over roles; a user-permission or state denial is never overridden by a role.

### 3. Role assignment carries scope and time

1. The unit of access is the **role assignment**: `(user, role, scope, valid_from, valid_to, source)`.
2. **Scope** is a set of value restrictions on link fields, using the existing user-permission mechanism ("these Company values", "these Warehouse values", with `applies_to`), **bound to the assignment** rather than to the user as a whole. The same person can be Accounting Manager for Company A and Accounting Viewer for Company B. This is the Oracle data-role / Workday constrained-group pattern, and it avoids the Odoo trap (global groups).
3. The platform stores scope as opaque link values. It does not know what a "company" is (ADR-058 rule 2); the ERP layer decides which links are scope dimensions (organisation units, ADR-059).
4. A user's effective rights are the union of the rights of their **active** assignments; the engine evaluates each assignment's scope together with its role, never mixes one assignment's role with another's scope. `explain` returns the assignment that granted or denied.
5. Assignments may be **time-boxed** (`valid_to`); expiry needs no admin action and writes an event.
6. **Period scope** (Dynamics' "modify current period only") is not a platform concept: the accounting period lock belongs to the ERP and is enforced as a lifecycle validation, not a role attribute. Recorded here so nobody adds it to the engine.

### 4. Assignment sources and conflict between them

Every assignment records its **source**; sources never silently overwrite one another.

| Source | Meaning |
| --- | --- |
| `manual` | An administrator assigned it, with a reason |
| `rule` | An assignment rule fired from business data (for example an HR position record, a department, a user attribute). The ERP supplies rule conditions; the platform stores and runs them |
| `idp` | Mapped from an IdP group or claim (OIDC claim, SAML attribute, SCIM group) |
| `emergency` | Granted through emergency access (rule 10) |

A source that manages an assignment may remove it; it may not remove an assignment from another source. If the same role arrives from two sources, both are kept, and access ends only when all end. A tenant can declare a role **IdP-managed**, which makes manual changes to it refuse with a clear message.

### 5. Authentication, login and sessions

1. **Methods:** local password (Argon2id, policy, breach-list check) with TOTP and recovery codes; OIDC; SAML 2.0; API keys and OAuth client credentials for services. All build on the `spot-auth` primitives (WP-PLT-02).
2. **Tenant policy** chooses: local only, SSO optional, or **SSO required**. When SSO is required, local sign-in is refused for every user except the **break-glass administrators** (rule 4.3). At least two break-glass administrators must exist while SSO is required. They sign in with a local password plus a hardware-bound or TOTP second factor, and every use raises an alert and an audit event. This is NetSuite's lock-out lesson applied: the IdP must never be the only way in.
3. **A session may not move to a weaker method.** A session established through SSO cannot switch to a role or user that does not allow SSO; step-up (re-authentication) is required for the sensitive operations listed by policy (role changes, API key creation, data export, emergency access).
4. **Session management:** a user can list and end their own sessions; an administrator can end any user's. Idle and absolute timeouts are tenant policy. Every session records the method, source address and device label.
5. **API keys and tokens belong to a user** (normally a `service` user). Their scope is a **subset** of the owner's rights, set at creation, never wider; a key stops working the moment its owner is deactivated.
6. **Portal users** authenticate like internal users (SSO is rare for them; magic link plus 2FA is allowed) but have no desk roles. Their access comes from **party scoping**: a portal role grants rights only to records that link back to the party (customer or supplier) on the user's party link. The decision core already supports this through `if_owner` and value restrictions; the party link is the user-permission value.

### 6. User lifecycle

1. **Joiner:** created by invitation (e-mail with a single-use, time-limited link), by SCIM `POST /Users`, or by first SSO login when the tenant allows just-in-time creation (default: off; JIT users get no role until a rule or an administrator grants one).
2. **Mover:** a change to the linked business record (position, department) re-evaluates assignment rules; stale rule-sourced assignments end, new ones begin, and an event records both.
3. **Leaver:** `enabled=false` (manual, or SCIM `active=false`, or an HR leaving date reached) must, **within one second on the node that handled it and within the sync freshness bound everywhere else**:
   - end all sessions and refresh tokens;
   - disable API keys;
   - cancel pending invitations;
   - reassign or flag open tasks and approvals (BPM, WP-BPM-05) to the user's manager or a nominated delegate;
   - write one audit event.
   Documents, comments and audit rows stay and keep showing the former user's name.
4. **Branch and offline nodes** (ADR-062 to ADR-067; the offline design is rule 12.13): a deactivation is an **obligation with a deadline** under ADR-065, with a **tenant-configured** maximum staleness for the user directory. The platform hard-codes only a floor (1 minute) and a ceiling (7 days); the shipped default (24 h) is a setting, and a tenant may set a shorter bound for sensitive roles (for example 15 minutes for roles that post or approve payments). A node that cannot reach the freshness bound refuses logins of users it cannot confirm, rather than trusting a stale "enabled".
5. **Recovery:** password reset and 2FA reset are self-service where policy allows; otherwise an administrator-initiated reset needs step-up, a second approver for administrators, and notifies the user on every address on file.
6. **Last-admin protection:** the system refuses to deactivate, demote or expire the last tenant administrator or the last break-glass administrator.

### 7. Federation: OIDC, SAML, SCIM

1. **OIDC and SAML** are first-class per tenant, several providers allowed, with the discovery and metadata documents the standards define. Linked identities are keyed by issuer and subject.
2. **SCIM 2.0** (RFC 7643 and RFC 7644) is served per tenant for `/Users` and `/Groups`, authenticated with a dedicated bearer credential owned by a `service` user. `PATCH active=false` is deactivation, never delete. Groups map to roles through an explicit **group-mapping table** `(idp_group → role, scope)`. We choose mapping by table rather than by custom attribute; changing the model later would mean re-mapping every customer's directory (the standard guidance), so this is decided now.
3. The mapping decides **assignments with source `idp`**; the IdP never edits roles themselves.
4. Just-in-time creation and group-mapping both write the audit events of rule 8.

### 8. Segregation of duties, review and audit

1. **SoD rules** (WP-SEC-02) are records over **duties** (and over roles for convenience): "no user may hold duty X and duty Y in the same scope". They are evaluated **when an assignment is created or changed**, before it takes effect. A conflict blocks the grant unless the grantor holds the `override_sod` right and records a reason and a **mitigating control**; every override is itself an auditable record and appears in the access review. Reference duties are authored without internal conflicts (the Workday finding).
2. **Approval of grants:** a tenant can require approval for chosen roles or duties. The approval runs on the BPM engine (WP-BPM-05), not a private workflow.
3. **Access review:** a campaign (WP-SEC-02) lists, per role owner or manager, every active assignment with its source, last use and SoD overrides; the reviewer keeps or removes each; unreviewed items past the deadline are escalated or, if the tenant chooses, removed. "Last use" comes from the audit log, so unused access is visible.
4. **Audit events** (topic class `identity`, journalled, ADR-056 and ADR-061): user created, enabled or disabled; role, duty or grant changed (with before and after); assignment created, changed, expired or removed (with source and reason); sign-in success and failure; lockout and unlock; session ended; API key created or revoked; SSO policy changed; SoD override; emergency access started, ended and reviewed; impersonation. Credentials, tokens and secrets never appear in an event.
5. **Answerable questions** are part of the product, not reports someone builds later: *what can this user do and why*, *who can do this and why*, *who has access to this document*, *what changed in access between two dates*. They use `explain` from the decision core.

### 9. Emergency access and impersonation

1. **Emergency access** is a pre-defined, **time-boxed** assignment of a named elevated role (`emergency` source). The user raises a request with a reason; the role owner approves; the session runs with **full action logging**; on its end the log goes to a **controller** (a person other than the user) who must confirm or flag it. Unconfirmed sessions escalate. This follows the SAP firefighter pattern. It is separate from platform support break-glass (WP-ADM-11), which is the vendor's access to the customer; they share the session-recording component.
2. **Impersonation** ("sign in as") exists only for **support-tier** principals (WP-AI-06, WP-ADM-11) with a customer-approved, time-boxed grant, never for tenant administrators over their own users. The action trail records **both** identities. Impersonating a user never gives more than that user has, and never the break-glass or administrator roles.

### 10. Administration surface

1. Everything above is reachable through the **typed native API** and metadata (so the admin UI, the CLI and scripts do the same thing, ADR-044), and by role-protected framework DocTypes: `User`, `Role`, `Duty`, `Role Assignment`, `Assignment Rule`, `IdP Provider`, `IdP Group Mapping`, `SoD Rule`, `Access Review`, `Emergency Access Grant`, `Session`.
2. **Who may administer whom** is itself permissioned: separate reference duties for *user administration* (create, deactivate, reset), *role administration* (edit custom roles), *assignment administration* (grant, within delegated scope) and *security administration* (SSO, SCIM, policy, SoD). A single person holding user administration and security administration is flagged by the delivered SoD rules.
3. **Delegated administration:** an assignment grantor may be limited to roles and scopes they hold themselves (a company's finance lead grants roles inside that company only).
4. Platform infrastructure administrators (ADM, control plane) and tenant administrators are **different principals**; neither implies the other (ADR-044's separate infrastructure and ERP roles).

### 11. People who work across companies, groups and franchisees

Three different situations, three different mechanisms. The platform never gives a person a "super user" for them.

1. **Several companies inside one tenant** (a group head over the group's own entities). One account, several **assignments**, each with its scope: Group Finance Head for Companies A, B and C, or for "Company A and everything below it" using the include-descendants option of the user-permission mechanism (the same idea as Workday's "include subordinates" and Dynamics' organisation hierarchy). One role with a broad scope, or different roles per company, are both expressed the same way (rule 3). Consolidated and cross-company reports honour the scope, so a user sees only the companies assigned.
2. **A franchisee is normally a different tenant** (ADR-058: the tenant is the unit of ownership and residency; the franchise link exchanges agreed records). The group head therefore gets franchisee information through the **link**, not through access to the franchisee's database: the agreed records (sales summaries, royalty bases, stock positions, whatever the franchise agreement lists) arrive in the franchisor's tenant and are protected there by the franchisor's own roles. The franchisee decides what the link shares and can narrow or stop it; the franchisor cannot read anything else.
3. **When someone really must work inside the franchisee's tenant** (an auditor or area manager): the franchisee **invites them as an external user** in its own tenant, with a franchisee-defined role, scope and expiry, signed in through the person's own company IdP (federation, rule 7) so there is no second password. It is a separate account in that tenant, created, reviewed and revoked by the franchisee, and appears in the franchisee's access reviews and audit log. The control-plane identity link (Not decided, item 1) would only let the person switch between the two tenants in one screen; it grants nothing.
4. **Not offered:** one login that implicitly carries rights into every tenant, or a franchisor administrator with standing access to franchisee data. Both break the residency and ownership boundary and the franchisee's ability to prove who can see its books.

### 12. Field-level access

Researched on 6 Oct 2026 (Appendix A). The survey corrects a first impression: tier-1 products do not all work through groups of fields. Dynamics 365 and NetSuite secure **individual fields**; Workday and Oracle secure **groups** (a domain, a content section); SAP does both (field values in authorization objects, plus field masking). All of them agree on the points below, and the recurring failures all come from the same gap: **the control is applied in one channel and the data leaves through another**.

1. **The unit is the field or a protection group.** A DocType can name **protection groups** (for example `compensation`, `cost`, `credit`, `bank_details`), and any field may be secured on its own or through a group. A secured field or group is **closed by default**: no grant means hidden (Dataverse; Dynamics' deny). Fields in no group follow the DocType right. `permlevel` (ADR-068, WP-PLT-03 T1) stops being the mechanism; it remains only as an import alias.
2. **Rights per field or group**, as a ladder (a higher right implies the lower ones, except where stated):
   - `read_masked`: sees a masked form (format set on the field: last four characters, a range, a fixed placeholder). The real value never leaves the server.
   - `read`: sees the value on a document.
   - `reveal`: sees the unmasked value of a masked field, **one document at a time**, each reveal audited. A separate `reveal_bulk` right exists for back-end services only (Dataverse's "one record" versus "all records").
   - `create`: may set it when the document is created (Dataverse and Dynamics separate create from update, for example a field set once at creation and then fixed).
   - `update`: may change it later; implies `read`.
   - `report`: may use it in list columns, reports, exports, print and search results. Oracle and NetSuite both carry a report/search level apart from the record level.
3. **Explicit deny wins.** A field grant may be a `deny`, which no grant from any other role, duty or assignment overrides (Dynamics: "deny always overrides"). Between grants, the most permissive one wins (NetSuite's highest-wins rule; our union rule). Each assignment applies with its own scope.
4. **The tenant administrator has all rights** (the architecture owner, 6 Oct 2026: restricting the administrator is an unnecessary limit). The `Tenant Administrator` reference role grants every right, including every protected field and group, and `reveal`. It is an ordinary assignable role, not a hidden bypass: its actions are audited like anyone's, field access trace and reveal logging still record what it reads, and a tenant that wants a narrower administrator may build a custom role. What stays true for everyone, administrator included: derived-field inheritance (rule 5) and no-inference-through-queries (rule 7) are properties of the data layer, not of a role. **System actors** (jobs, sync, migrations, hooks) are not people: they declare in their manifest which protected groups they read or write, and the engine enforces that declaration; an undeclared access fails and is logged.
5. **Derived values inherit the strictest protection of their inputs.** Every surveyed product has a hole here: Dataverse calculated and composite columns, NetSuite formula fields, Dynamics display methods, Oracle reports that do not enforce section security. A computed field, a rollup, a formula and a report column that reads a secured field are **secured by the same group** unless the author declares the output as deliberately declassified (with a reason, reviewed). A lint rejects a derived field that reads a secured field and carries no protection and no declassification.
6. **One enforcement point.** Field access is decided in the data and API layer, not in a view or a screen (ADR-068: hidden is not security; SAP's UI masking is documented for its UI channels and is the cautionary example). It applies to every path that returns data: document read, list, report, export and print, realtime events, journal stream, link lookups and titles, search index and embeddings (WP-AI-10), version history and audit diffs, API and SCIM output, and notifications.
7. **No inference through queries.** A user may **filter, sort, group or aggregate** on a field only if they can `read` it unmasked. A masked or hidden field in a filter is refused (`permission.field_denied`), not ignored, so an aggregate or an ordered list cannot leak it. The same applies to counting or ranking against a secured field.
8. **Writes name the field.** A payload that changes a field the user cannot `create` or `update` is rejected with `permission.field_denied` and the field path. It is never silently dropped. An unchanged echo of a hidden or masked field is accepted so edit forms work.
9. **Audit.** Create and update audit rows for a secured field store the masked form; the real values live only where the field itself lives. Groups tagged `trace` also write a **field access trace** (who, when, which document, whether authorised, through which API; SAP's field access trace) on every `read` and `reveal`, and every `reveal` is traced whatever the tag.
10. **Classification tags** (`pii`, `financial`, `secret`, `health`) are separate from groups. They grant nothing; they set the default protection of a new field, drive review reports, privacy requests and export rules, and a field carrying `secret` or `pii` and no protection fails the lint unless declared.
11. **Conditions.** A field grant may carry a condition on the **document** or the **user's own attributes**: own record only (a user sees their own salary), or a status ("credit limit editable only in draft"). Document state rules that already belong to the lifecycle (ADR-51) stay there.
12. **Per-record field sharing** (Dataverse's field sharing: giving one person access to one secured field on one record) is the natural extension of `Share` and is **deferred**: designed in, not built in v1.
13. **Branch and offline nodes.** Decided 6 Oct 2026 (the architecture owner), on the research report `docs/adr/research/offline-access-research.md`. A branch may run the full system offline and so may legitimately need protected fields while disconnected. The research found that no tier-1 vendor documents offline revocation, offline key custody or lost-device handling, so the design below is derived from adjacent systems (TUF, read-only domain controllers, Couchbase, Vault leases, Entra continuous access evaluation) and is marked as a design, not a copied practice. It builds on ADR-062 to ADR-067 and extends ADR-065 (freshness) and ADR-067 (`QUARANTINED`).

    **a. Sync classes** (set per protection group and tenant):
    - `everywhere`: the value syncs like any field; the node enforces the same grants offline. **The platform default.** A group tagged `secret`, `pii` or `financial` has no default: the tenant must choose a class explicitly.
    - `encrypted`: stored encrypted on the node; its key is released to the node's memory under a lease (item e).
    - `sealed`: the node can **create** values but can never read them back; each value is sealed to a key only head office holds. For entry cases such as bank details and salary lines.
    - `home_only`: does not sync to branch nodes; shows as unavailable there.

    **b. Signed permission Snapshot.** The hub publishes per node a signed Snapshot: tenant, node id, a **monotonic epoch**, `issued_at`, a `valid_until` per tier, the compiled grants, the user directory with `enabled` flags and the per-user offline allowlist, the sync-class table and the current key epoch. Signing roles are split as in TUF (an offline root key, an online snapshot key, a small frequently re-signed timestamp message that ADR-065's heartbeat can carry). The node stores the highest epoch it has accepted and **rejects** a lower epoch, an equal epoch with a different hash, and anything past `valid_until`. This is the rollback and freeze defence.

    **c. Lease and fail-closed.** `valid_until` is measured with the node's monotonic clock (ADR-065) and the **remaining lease is persisted across reboots**, so a restart can never extend it. When a tier's bound lapses, that tier fails closed: users the node cannot confirm cannot sign in (rule 6.4), and the tier's actions are blocked; lower tiers continue until their own bound. A supervisor holding `sync.override_stale` may override, audited (ADR-065).

    **d. Risk-tiered bounds**, tenant-adjustable within the floor (1 minute) and ceiling (7 days) of rule 6.4. Shipped defaults: **Tier 1** 15 minutes to 1 hour (payment posting, approvals, administrator actions, `reveal`); **Tier 2** 12 hours (reads of protected groups); **Tier 3** 24 hours (ordinary work). These values are proposals anchored on NIST SP 800-63B-4's 24 h and 12 h session limits, not findings. A group's key lease (item e) equals its tier bound.

    **e. Keys.** Disk encryption on branch machines is a **baseline only**: TPM-bound without a PIN by default (the only mode that reboots unattended), with a network-bound option (Tang) where a LAN server exists. It stops a stolen powered-off disk, not a running or auto-unlocking machine. Real protection sits above the disk: a tenant root key in head-office key management, one data key per protection group, and a per-node wrapping key, so the hub can stop releasing keys to one node without touching others. Fields use randomised authenticated encryption, with no index on salary-type fields (a blind index only on lookup keys). Data keys live in process memory only (locked, zeroized, no core dumps), never in the database or logs, and are released under the lease of item d. Revoking a node means stopping key release **and rotating the data keys of the groups it held**.

    **f. Reveal is online-only.** `read_masked` works offline against the stored masked form. `reveal` and `reveal_bulk` need a live call to the hub, which releases the value and audits it. Offline, the screen says why; the supervisor override applies only where the group's class allows it.

    **g. The tenant administrator offline.** The administrator's credential is **not cached on branch nodes by default**, and administrator actions need a live connection (step-up online), as read-only domain controllers deny Domain Admins. A tenant may enable offline administration per node. This limits **where** the administrator can sign in offline, not **what** the administrator may do (rule 4 stands).

    **h. Node identity and scope.** Every node has its own mutual-TLS identity and replication role, scoped to its own data, with a short-lived certificate that renews while connected and lapses on its own if the node stays offline. Suspending or decommissioning a node rejects its connections, stops key release, and publishes a revocation entry that other nodes learn from their next Snapshot. Sync scope is a **group-by-node allowlist with deny overriding allow**, empty by default for tagged groups. Every channel of rule 6 honours the class: the journal stream (ADR-061), bootstrap snapshots (ADR-066), search and embeddings, audit diffs, reports, caches and backups must exclude or seal the data of `home_only`, `encrypted` and `sealed` groups, because one leaking channel defeats the class.

    **i. Quarantine, not loss.** Each offline write carries author, role-assignment id, epoch, epoch age and a device-local sequence number, in an append-only, tamper-evident local journal. On sync the hub compares with its revocation records. Actions within their tier bound are accepted and **flagged**; actions by a revoked user, beyond the bound, or in Tier 1 after revocation are **quarantined** for a person (ADR-067), never silently dropped.

    **j. Card data.** Card account numbers are **never stored on any node**, branch or hub, in the platform; payment goes through the payment provider's tokens. This keeps branch nodes outside the heaviest card-data obligations.

    **k. Detection.** Decoy ("honey") rows per node reveal which stolen node was opened; unusual pull volume from a node raises an alert. Detection only: remote wipe cannot reach an offline machine.

    **What this does not give, and says so:** revocation on a disconnected node can never be faster than its bound; a node that holds plaintext can be read by whoever controls it; and application policy cannot protect a field from the node's own host. For those cases the class is the answer (`sealed` or `home_only`), not the lease.

### 13. Where each piece lives

Work packages are in `docs/blueprint/platform.md`, workstream WS23 (ADR-070 wins where they differ).

| Concern | Crate or area | Work package |
| --- | --- | --- |
| Credential primitives | `spot-auth` | WP-PLT-02 (T1 exists) |
| Decision core, snapshot, explain | `spot-permissions` | WP-PLT-03 (T1 exists) |
| Query compilation | `spot-permissions` + query IR | WP-PLT-04 |
| Identity events and the `via` amendment | events contract | WP-IAM-01 (lead) |
| User directory, lifecycle, invitation, recovery | `spot-iam-directory`, `spot-iam` | WP-IAM-02 (T1 briefed) |
| Role, duty and scoped assignments; snapshot publication; packs | `spot-iam-roles`, `spot-iam` | WP-IAM-03 (T1 briefed) |
| Assignment sources and rules | `spot-iam` | WP-IAM-04 |
| OIDC, SAML, sign-in policy, break-glass, step-up | `spot-iam-sso` | WP-IAM-05 |
| SCIM and group mapping | `spot-iam-scim` | WP-IAM-06 |
| Sessions, API-key scoping, impersonation | `spot-iam`, `spot-auth` | WP-IAM-07 |
| Grant-time SoD, approval, emergency access, answerable questions | `spot-iam`, BPM | WP-IAM-08 (with WP-SEC-02) |
| Offline identity for nodes (rule 12.13) | `spot-iam`, sync | WP-IAM-09 |
| Admin UI | UI stage | WP-IAM-10 |
| Field-level access (section 12) | `spot-permissions` | WP-SEC-04 (now core lane) |
| Protected-group keys, sealed fields | `spot-secrets` | WP-SEC-10 |

## Consequences

- **Contracts.** `Actor` (events contract) gains one additive field for the impersonating identity (`via: Option<RecordId>` on `Actor::User`), and the events contract gains the `identity` topic class. Both are additive and need the architecture owner's approval as an amendment (contract v0.x). Nothing else in the frozen contracts changes. A new `contracts/identity` is **not** proposed; the IAM DocTypes are ordinary metadata.
- **WP-PLT-03 T1 stays valid**: it already evaluates roles, user permissions, shares and owner rules as a pure core. Two follow-ups, to be added as tasks under WP-PLT-03: (a) `Snapshot` built from *assignments with scope* rather than a flat user→role→permission table; (b) the `Duty` layer. Neither changes the T1 brief.
- **ADR-068 and WP-PLT-03 T1 change for field access.** `permlevel` is replaced as the authorization mechanism by field and protection-group grants (section 12). **Done 6 Oct 2026:** ADR-068 amendment A1 written (proposed), WP-PLT-03.T1 revised (field grants, per-assignment evaluation), WP-CON-02.T3 briefed for the contract change (app API 0.3.0). A follow-up WP-PLT-03 task covers field grants, masking, the enforcement hooks and the derived-field lint. WP-SEC-04 becomes the masking, reveal and trace task.
- **WP-SEC-02 and WP-SEC-04** are unchanged in purpose; WP-SEC-02 now reads duties and assignments from WP-IAM-02.
- **Sync.** User, role, duty, assignment and group-mapping records are **tenant-wide, HQ-owned policy data**, routed by ADR-063 to branch nodes read-only. Deactivation is a freshness obligation (ADR-065). Sessions and sign-in events are node-local and journalled upward. To be confirmed in the sync work packages.
- **Security review.** Adds attack surface: SCIM bearer endpoint, SAML parsing (XML signature wrapping is the known danger; a vetted library, no hand-written XML-DSig), account recovery. Each gets an entry in `docs/security/threat-model.md` and a test list in its brief.
- **Cost.** Ten new work packages (WP-IAM-01 to 10), one more security package (WP-SEC-10) and a UI stage. They fit the open stages (backend, ui) and do not touch Foundation or ERP.

## Not decided here (for the architecture owner, or later ADRs)

1. **Cross-tenant identity convenience** (the control-plane identity link): kept as an option only. A later ADR decides it if consultants and group accountants ask.
2. **Passkeys / WebAuthn** as a primary method: the primitives should leave room; scheduled as an addition to WP-IAM-06, not v1.
3. **Org-structure-driven scope** (position hierarchies, "my team's records"): the ERP side belongs with HR design, closed until Foundation opens. The platform exposes a hook (scope values from a query) and nothing more.
4. **Customer-facing (CIAM) scale** for portals with very large numbers of users: v1 targets portal users in the thousands, not millions.

## Open questions for the architecture owner

1. ~~Approve the tenant-owned model (option C)~~ **Approved 6 Oct 2026 (the architecture owner)**, and confirm that a single global login across tenants is out of scope for now.
2. ~~Approve the Duty layer~~ **Approved 6 Oct 2026 (the architecture owner)**: between role and grant (Dynamics, Oracle and SAP all have it). The alternative is roles of raw grants, which is simpler to build and much harder to review and upgrade.
3. ~~Approve scope bound to the assignment~~ **Approved 6 Oct 2026 (the architecture owner)** (not to the user).
4. ~~Approve the `Actor` amendment~~ **Approved 6 Oct 2026 (the architecture owner):** `via` on `Actor::User` and the `identity` topic class.
6. ~~Field-level access (section 12)~~ **Approved 6 Oct 2026 (the architecture owner)**, with two changes: the tenant administrator has all rights (rule 4), and the offline design (rule 13) was **decided 6 Oct 2026 on the research report**: default `everywhere` with a forced choice for tagged groups, the new `sealed` class, tiered bounds (tenant-adjustable), administrator not cached offline by default, no card numbers on any node. Derived-field inheritance and the replacement of `permlevel` are approved.
5. ~~Staleness default~~ **Decided 6 Oct 2026 (the architecture owner): configurable, not hard-coded** (rule 6.4: a floor, a ceiling, and a per-tenant setting, optionally tighter for sensitive roles).

## Appendix A: field-level access in the tier-1 products

| Product | Unit | Access levels | Notable behaviour | Known gaps |
| --- | --- | --- | --- | --- |
| **Dynamics 365 / Dataverse** | Column, opt-in per column; column security profiles | Read, Read unmasked (one record or all), Update, Create | Masking by pattern rule; secured-and-unassigned shows as masked/locked; unmasked reads audited; audit before/after values masked; **field sharing** per record | Lookup, formula and system columns cannot be secured; calculated and composite columns leak unless also secured; system administrator is exempt; users need a refresh after changes |
| **Dynamics 365 Finance & Operations** | Table field, inside a permission that also reaches menu items and tables | Read, Update, Create, Delete (hierarchical) | Access flows entry point to table to field; **explicit Deny overrides every grant** | Display and edit methods cannot be secured per field; forms with several data sources fall back to the entry-point permission |
| **SAP S/4HANA** | Authorization object fields (organisational values, activity such as read or write); HCM infotype and subtype with levels R, M, W, E, D, S; UI data protection masking per field; CDS access conditions for rows | Display, change, and activity levels; masking: mask, clear, hide or disable | Masking is role-based or attribute-based; **reveal on demand**; **field access trace** records who viewed a protected field | Masking is documented for the UI channels (SAP GUI, Web Dynpro, Fiori); coverage of APIs is not stated in the sources found, so treat it as a gap to design against |
| **Workday** | **Domain**: a collection of tasks, reports, report fields, data sources and web services secured together | View, Modify; for integrations Get, Put; constrained or unconstrained by organisation | UI and integrations are secured separately; access follows the domain | Granularity is the domain, not the single field; delivered groups may carry internal conflicts |
| **Oracle Fusion** | **Content section** of a person profile; attribute groups in data security policies | View, Edit, Report | Policies and privileges are generated when a content type is mapped to a role; roles must be regenerated afterwards | Section security is not enforced in some analyses and reports (best-fit, comparison, transactional BI) |
| **NetSuite** | Custom field, per role, department and subsidiary | None, View, Edit, Run (Run: visible in search and reports, not on the record) | **Highest level wins**; defaults for anyone not listed; applies to records, searches, reports, mail merge and dependent formula fields | Removing the administrator's access breaks admin-run scripts |

Patterns carried into section 12: field or group as the unit; closed by default; create separate from update; a report right; masking with audited reveal; explicit deny; highest-grant-wins between grants; field access trace; one enforcement point; derived values inherit protection. Failures avoided by design: the UI-only control, the derived-field leak, and the report that skips the check.

## Sources

- SAP: [user administration and role maintenance](https://www.consilio-gmbh.de/en/glossary/sap-authorizations); [emergency access management](https://blog.sap-press.com/emergency-access-management-with-sap-grc); [user access review in SAP Access Control](https://learning.sap.com/learning-journeys/discovering-the-main-functionalities-of-sap-access-control/performing-user-access-review_f0a1d920-abe1-4637-9a1f-5dc9b84b37ad)
- Microsoft: [Role-based security in Dynamics 365 Finance and Operations](https://learn.microsoft.com/en-us/dynamics365/fin-ops-core/dev-itpro/sysadmin/role-based-security); [create new users](https://learn.microsoft.com/en-nz/dynamics365/fin-ops-core/dev-itpro/sysadmin/tasks/create-new-users)
- Oracle: [Role-based security in Oracle Fusion Cloud](https://docs.oracle.com/en/cloud/saas/supply-chain-and-manufacturing/26a/fassc/role-based-security-in-oracle-fusion-cloud-scm.html); [Security reference implementation](https://docs.oracle.com/en/cloud/saas/public-sector-compliance-regulation-common/22b/pssec/security-reference-implementation.html); NetSuite: [SAML single sign-on](https://docs.oracle.com/en/cloud/saas/netsuite/ns-online-help/chapter_N3825119.html), [permissions requiring two-factor authentication](https://docs.oracle.com/en/cloud/saas/netsuite/ns-online-help/section_1515446005.html)
- Workday: [separation of duties: roles and security in Workday Financials](https://support.stevens.edu/support/solutions/articles/19000154094-Separation-of-Duties-Roles-and-Security-in-Workday-Financials); [designing and configuring Workday security](https://tcblog.protiviti.com/2020/09/17/4-ways-to-effectively-design-and-configure-workday-security)
- Field level: [Dataverse column-level security](https://learn.microsoft.com/en-us/power-platform/admin/field-level-security); [masking rules](https://learn.microsoft.com/en-us/power-platform/admin/create-manage-masking-rules); [column-level security with code (field sharing)](https://learn.microsoft.com/en-us/power-apps/developer/data-platform/column-level-security); [field-level security in D365 Finance and Operations](https://alexdmeyer.com/2019/07/08/field-level-security-in-dynamics-365-for-finance-operations/); [SAP UI data protection masking](https://help.sap.com/docs/UI_DATA_PROTECTION_MASKING_FOR_S4HANA/a884fc9a091748c9a1f2a8c376dcd5bf/8111d2f06182443683853b7e03eb0436.html); [SAP HCM authorization checks](https://learning.sap.com/courses/authorizations-in-sap-hcm-for-s-4hana/outlining-hcm-authorization-checks); [Workday configurable security](https://doc.workday.com/workday-education/en-us/course-manuals/creating-and-securing-integrations/workday-configurable-security.html); [Oracle: secure content sections in person profiles](https://docs.oracle.com/en/cloud/saas/human-resources/ochus/how-you-secure-content-sections-in-person-profiles.html); [NetSuite: restricting access to custom fields](https://docs.oracle.com/en/cloud/saas/netsuite/ns-online-help/section_N2841053.html); [NIST SP 800-162 (ABAC)](https://csrc.nist.gov/News/2014/SP-800-162,-Guide-to-ABAC-Definition-and-Considera)
- Odoo: [security guide: access rights, record rules and multi-company limits](https://medium.com/@niralchaudhary9/odoo-security-complete-guide-to-access-rights-record-rules-field-level-security-e0e3c878f08f)
- SCIM: RFC 7643 and RFC 7644; [SCIM provisioning for SaaS: implementation guide](https://ssojet.com/blog/scim-provisioning-saas)

The vendor pages were read through search summaries and one full page (Dynamics); the product behaviour listed in the comparison table should be re-verified against vendor documentation before any detail is quoted outside this ADR.
