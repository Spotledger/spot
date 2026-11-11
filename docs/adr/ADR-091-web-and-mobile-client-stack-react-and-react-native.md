# ADR-091: Web and mobile client stack: React for the web, React Native for mobile, one Rust core underneath

- **Status:** **Accepted by lead under the architecture owner's full authority, 31 Oct 2026** (recorded in the decision log, E0; the architecture owner decided it in principle on 27 Oct 2026 and may overturn it). Section 5 "proposed defaults" are accepted at their proposed values. Originally **Proposed** by the lead, 27 Oct 2026, on the architecture owner's decision in principle of the same day. **The architecture owner decided in principle (27 Oct 2026) to DROP Dioxus**: the web runtime is React, mobile is React Native, both used as the community uses them (*"we bring the entire React as it is, as people use it, we are not doing our custom engineering; our main point is to make it work with our Rust core"*). His reasons: battle-tested at large scale, less worry, mobile becomes a main thing very soon. This document turns that into a reviewable ADR. **Everything in section 5 marked "proposed default" is for the architecture owner to confirm; sections 10 to 12 hold the slate and the open questions.** Nothing here is registered in `plan/`; no code, no contract edit, no edit of another ADR (section 3 lists the sentences that need amending and leaves them untouched).
- **Date:** 27 Oct 2026
- **Layer:** Platform and UI (the web and mobile clients). No Foundation or ERP design (stages closed, ADR-060).
- **Number:** 091. Nothing in the text depends on the number.
- **Builds on:** ADR-084 (server-hosted UI components, view trees), ADR-086 (third-party JavaScript policy, and its hold amendment), ADR-088 (client data layer, option C), ADR-089 (rendering and caching, on hold), ADR-083 (consumer identity), ADR-069 (UI composition and compiled UI bundle), ADR-054 (scripting engine; "no Node or npm at runtime"), ADR-055 (native API), ADR-078 (transport bindings), ADR-060 (build order), the external-service-component ADR (ADR-090, parallel), SPK-15 (`docs/spikes/SPK-15/`).
- **Supersedes:** the Dioxus decisions of the WP-WEB-08.T1 brief (section 3.1).
- **Reading guide:** every claim about an outside system carries one of three tags. **VERIFIED** means I opened the source page on 27 Oct 2026 and the page says it. **ASSUMED** means it comes from a search-result snippet, a page that did not say it, or my own knowledge; it must be checked before anything depends on it. **MEASURED** means a number from this repository or from the architecture owner's own measurement, reproduced here, not re-run. Section 13 lists every source and everything that was not researched.

## 0. Summary

1. **Decision (proposed):** the web client is **React** (TypeScript), the mobile client is **React Native**, the desktop client stays **Avalonia**. All three are clients of the public APIs and all three bind **one sans-IO Rust core**, `spot-client-core` (ADR-088 option C, unchanged). The JavaScript runtime that serves the web client, if one is needed at all, is an **optional external service component**: not installed by default, listed in the Admin Center, enabled on demand, scaled separately, and an **untrusted client** of the hub. The hub (the Rust core engine) keeps authority, validation and permissions (section 9).
2. **What this costs, in one line:** the platform gives up "Rust only, no JavaScript ecosystem" (charter decision 6) for the client layer and buys the community's tooling, hiring pool, accessibility primitives and a credible mobile path; it takes on a JavaScript supply chain (section 7), and it takes on **the hard part: binding a Rust core to three runtimes, one of which (React Native's Hermes) cannot load WebAssembly** (section 5).
3. **The hard part is smaller than it looks, and riskier than it looks.** Smaller, because ADR-088's boundary is already narrow and serialisable (requests out, replies in, state observable); a binding is a few functions that move bytes, not an API per operation. Riskier, because the one maintained generator that targets React Native with TypeScript bindings says of itself that it "is still in early development, and should not yet be used in production" (VERIFIED, section 5.3). So the **first tasks are an evidence gate**, not a shell: the skeleton core and corpus driven from TypeScript over WASM, from a React Native native module, and from C# (section 5.6).
4. **Proposed defaults** (section 4): TypeScript; a standard router, state and data-fetching selection by a written popularity-and-maintenance rule; **Expo** for React Native; **pnpm** workspaces; **Next.js only if server-side rendering is needed**, and then only as the contained external component. With rungs 0 and 1 on hold, **no SSR is needed today**; the recommendation (open question 1) is to start with a browser-only app and decide on Next.js when the hold lifts.
5. **Supply chain** (section 7): standard practice only (lockfiles, install scripts off, scanning, update cooldown, provenance, CODEOWNERS on lockfiles) plus architectural containment (the component holds no authority). The architecture owner's lockfile snapshot of 27 Oct 2026 is reproduced as a measurement: React 3 unique non-optional packages; Next.js 16.4 + React 15; React Native 0.88 188; Expo 58 + React Native 397.
6. **Twelve proposed tasks** in build order (section 10). The first are the core-binding gate, then the web shell, then the React Native shell.

## 1. Context

### 1.1 What was decided before this ADR, and what that now means

| Earlier decision | Source | Standing under this ADR |
| --- | --- | --- |
| Web UI in Rust: Dioxus subset, forked, vendored, own toolkit, own router, in-process web role | WP-WEB-08.T1 brief, "Decisions already taken by the architecture owner (26 Oct 2026)" | **Superseded** (section 3.1) |
| Charter decision 6: *"Rust only; no JavaScript ecosystem in our stack. No Node, no npm, no JS framework, in the build or at runtime."* | `docs/research/web-runtime-charter.md` on `main`, section 2 | **Amended for the client layer** (the charter needs a follow-up edit; question 7). ADR-088 §4.2 already narrowed it on 27 Oct: JavaScript in the browser allowed, no server-side JavaScript runtime in the product. This ADR narrows it further: a server-side JavaScript runtime is allowed only as the optional external component |
| Client data layer: option C, one sans-IO Rust core, thin generated surface per platform, TS SDK for outside developers deferred | ADR-088, decided by the architecture owner 27 Oct 2026 | **Stays.** Its consumers change (section 5) |
| Server-hosted components emit view trees; each platform renders the control catalogue | ADR-084 | **Stays.** Three renderers instead of two (section 6) |
| Rungs 0 and 1 (static and server-rendered pages) on hold | the architecture owner, 27 Oct 2026; ADR-086 hold amendment; ADR-089 banner | **Stays.** It is what makes "no SSR today" possible |
| In-memory bearer, renewal cookie only, no token in a URL | ADR-088 §7.2 (accepted for now) | **Stays** (section 5.2) |
| Consumer = separate principal, edge of the same tenant; hub/edge vocabulary | ADR-083, 26 Oct 2026 | **Stays** |
| Avalonia is the staff desktop client | ADR-028, ADR-27, `clients/avalonia/` | **Stays** |
| "Customer mobile apps decided at W8 by SPK-09 (Avalonia, React Native, native)" | charter, work-order table; charter §8 question 8 | **Overtaken**: mobile is React Native (proposed). SPK-09 as a mobile-stack choice needs closing or rewording (a lead task) |

### 1.2 What SPK-15 measured, and what it does not say about React

SPK-15 compared three arms on one mock API: A (Dioxus), B (own Rust core), C (server-first with small TypeScript). The numbers that remain relevant, all MEASURED in the spike on an AMD Ryzen 5 4500U shared with other sessions (`docs/spikes/SPK-15/traffic/RESULTS.md`, as quoted in ADR-088 §2.1):

- S1 page weight, gzip-6: arm A 157 KB, arm B 131 KB, arm C 0 KB. S3: arm A 218 KB, arm B 131 KB, arm C 8 KB. Charter budget: S1 at most 250 KB, S3 at most 600 KB.
- 57 acceptance checks, 8 browser-free (14 percent); S3 has 0 of 12 browser-free.
- Arm A fork sizing: 25 upstream crates, 63,270 lines of Rust, one trial on 0.8.0-alpha.3 failing 7 of 47 browser checks (cause not found).

**No React arm was built.** SPK-15's evidence therefore says nothing about React's page weight, hydration behaviour or agent-friendliness. It stays as history for why Dioxus was dropped (the fork burden and the 0.8 failure are the evidence the architecture owner already had) and as a measured baseline: **React's gzip size against these budgets is unmeasured**, and the web-shell task (section 10, task 6) must measure it before the budgets are called met.

## 2. The decision and its reasons

### 2.1 Decision

1. Web: React with TypeScript.
2. Mobile: React Native with TypeScript.
3. Desktop: Avalonia, unchanged.
4. One Rust core, `spot-client-core`, behind a narrow serialisable boundary, bound three ways: WebAssembly via wasm-bindgen for the web, a native module for React Native, a native library for C#.
5. Standard community practice for everything above the core: **we use the frameworks as their communities do and write no framework of our own.** Our own code is the core, the generated contract surface, the control-catalogue renderers (small, section 6) and glue.

### 2.2 Reasons (the architecture owner's, restated; evidence where we have it)

1. **Battle-tested at large scale.** The cost of the Dioxus path was measured and is ours: a 25-crate, 63k-line fork we would carry, upstream issue 5427 hitting hydration, a 0.8 migration that failed 7 of 47 checks, and a toolkit (host helper, router, build command, agent guide) we would have had to write. React removes that class of work. (MEASURED, SPK-15 fork drill, as above.)
2. **Less worry.** Agents and humans know React; the failure modes are documented in public. This is a judgement, not a measurement: the SPK-15 agent trial that would have compared first-attempt pass rates was never run (ADR-088 §2.3).
3. **Mobile becomes a main thing very soon.** The Dioxus native renderer was a work in progress at the time of the charter (*"mobile through Dioxus means a webview for now"*, charter §9). React Native gives a real native UI path.
4. **Accessibility and right-to-left are first-class concerns of the platforms we adopt** (section 8.2), rather than something we would have to prove in a minimal Rust DOM subset.

### 2.3 Tradeoffs, stated plainly

| What we give up or take on | Honest size |
| --- | --- |
| A single language end to end | The UI is TypeScript; the core and server are Rust. Two languages, one FFI boundary. ADR-088 already accepted the boundary for Avalonia |
| A small dependency tree | See section 7. The architecture owner's lockfile numbers: React 3 packages, Next.js 16.4 + React 15, React Native 0.88 188, Expo 58 + React Native 397. The Rust-only path would have had a larger line count and a smaller, vetted-by-us tree. These counts exclude optional per-platform binaries and the dev tooling of apps, so real trees are larger |
| Rust-side hydration and render caching designed in ADR-089 | ADR-089 is on hold; if SSR returns, its render and cache layer moves into the external component or stays at the view-tree level (section 3.2) |
| A tested binding story | **We have none.** The web leg (wasm-bindgen from TypeScript) is the common path. The React Native leg is not (section 5.3) |
| Security record of the server layer | React Server Components had a CVSS 10.0 unauthenticated remote code execution (CVE-2025-55182, VERIFIED, section 7.1) and Next.js a middleware authorisation bypass (CVE-2025-29927, ASSUMED). Containment is architectural (section 9) |
| Node at runtime | Only if SSR returns, and only inside the optional component. A browser-only app has no JavaScript runtime on the server; Node is then a **build-time CI tool** only (ADR-088 Q2, accepted for now) |

## 3. What this supersedes and what needs amending

This ADR edits nothing. After the architecture owner accepts it the lead amends the documents below in separate PRs.

### 3.1 Superseded outright: the WP-WEB-08.T1 brief's Dioxus decisions

All taken from the brief's "Decisions already taken by the architecture owner (26 Oct 2026)" (the task brief), which are now void:

1. Rendering stack item 1: Dioxus forked and consumed as a narrow subset (core, signals, hooks, `rsx`, HTML elements, web and SSR renderers, `dioxus-history`, part of the fullstack core).
2. Rendering stack item 2: **the Dioxus fork policy** (no patches by default, follow upstream `main`, consume by pinned commit, upstream sync gated by the SPK-15 harness, 19-line patch for upstream issue 5427 as first candidate) and **the vendored directory** with its pipeline exemption from the PR gate.
3. Rendering stack item 3: **the thin toolkit** (host helper crate owning the **hydration skeleton**, props embedding and shell; a `spot web` command on Rust `wasm-bindgen`; **our own small client router** on `dioxus-history`; the asset step; the agent guide) and **no `wasm-opt`**.
4. Rendering stack item 4 and integration rule 8: toolkit crates as core runtime crates, and **the web side calling the core as a direct in-process call by default** (the **in-process web role**). The web client is now a client of the public APIs (section 9).
5. Runtime shape: "a separate web process (scaled to N) ... in-process in standalone installs" is replaced by the optional external component; the slim-web-binary paragraph (three revisit triggers, measurement of 26 Oct) no longer applies.

**Kept from the same brief** (not Dioxus-specific): integration rules 1 to 7 as principles (one definition store, one set of semantics, one identity path, one process engine, one event stream, one test gate), read with "web runtime" meaning the contract between hub and client; the hub/edge vocabulary; the "who contributes UI" rules; the task of writing the consolidated web-runtime ADR, which becomes a short tying-together document after this one is decided (its "reference workload check" and "decide the rendering stack" deliverables change).

**SPK-15 Dioxus evidence stays as history** in `docs/spikes/SPK-15/` and in ADR-088 §2. Nothing there is deleted. Its harness (Playwright, the S1 to S5 slice checks, `?lang=ur`, hydration-mismatch checks, `spk15-nightly.yml`) is framework-neutral and **stays** (section 8.3).

### 3.2 Sentences in other ADRs that need amendment

| Document | Sentence or section (quoted or located) | Needed change |
| --- | --- | --- |
| ADR-084 | §0 item 5: *"The same tree is rendered by both the web runtime and Avalonia from the same catalog implementation"* | Three renderers (React, React Native, Avalonia); "catalog implementation" is a conformance corpus, not shared code (section 6) |
| ADR-084 | §8 items 4 and 5: SSR uses *"the host helper's hydration skeleton"*; hydration *"removes the mismatch class that the charter worries about for the Dioxus fullstack path (charter section 9, upstream issues 5427, 4631)"* | SSR (if it returns) is done by the external component with React; hydration mismatch is still possible with React, so the SPK-15 mismatch gate stays; the Dioxus reference goes |
| ADR-084 | §9 and the "one tree, two renderers" corpus text; slate row T7 (*"Web runtime renderer, SSR and hydration of trees ... WP-WEB-08 slate"*) | Add the React Native renderer to the corpus runs; T7 re-pointed to the web shell task of section 10 |
| ADR-084 | §13 and the `widget` node: *"web only"*, fallback for renderers without web widgets | Unchanged in substance; React Native joins Avalonia as a fallback-rendering platform |
| ADR-086 | §0 item 3 and §6 (*"The stack stays Node-free end to end"*, *"supply chain without Node"*) | Becomes: the **hub** stays Node-free; third-party browser files are still vendored by hash; the JavaScript client's own dependency tree is governed by section 7 of this ADR, not by the vendoring check. The two supply-chain regimes must be written side by side |
| ADR-086 | §3.3, *"The sink question for the chosen client stack is open. Whether DOM writes made by the WASM client (arm A's Dioxus subset) pass through a Trusted Types sink ..."* | The question is now about React DOM: whether React's DOM writes and the catalogue renderer pass Trusted Types without wrapping (ASSUMED that React writes through DOM property setters and not `innerHTML` except `dangerouslySetInnerHTML`; to be tested by the audit in ADR-086 §3.9). Trial item in its slate (task 2) re-pointed |
| ADR-086 | §5.4, widget budgets against SPK-15 headroom (93 KB on S1, 382 KB on S3, from arm A) | Headroom must be re-derived from the React shell's measured weight |
| ADR-088 | Status paragraph and Hold bullet: *"while the platform focuses on Dioxus"* | Read as "on React" |
| ADR-088 | §0 item 2: *"C is still chosen, because our web UI is Rust (Dioxus)"*; §4.2 table row "Language fit with the UI: Our UI is Rust (Dioxus) ... no FFI" | The language-fit argument for C over B is **gone for the web**: the web now crosses a JS/WASM boundary. C still wins on one implementation of the queue, cache and resync, on the measured disagreement of independent implementations (ADR-088 §2.2) and on three clients needing the same behaviour. Section 5.1 restates the argument |
| ADR-088 | §5.1 diagram: *"web: same wasm bundle"*; *"web (Dioxus, a Rust dependency with no FFI)"* | Web consumes the WASM core from TypeScript, with a generated TS typed surface for our own client (section 5.2) |
| ADR-088 | §3.3 and §5.3 *"Per-rung bundles ... the five-binary split arm A already builds"* | Obsolete; bundle budgeting is a property of the React shell and the code-splitting of the WASM module |
| ADR-088 | §8.3 first bullet: *"Mobile is decided to be a webview ... C's FFI tax is never paid"* and §11 Q7 *"mobile binding generator, deferred"* | Mobile is React Native, not a webview, so the FFI is paid on mobile; Q7 is **no longer deferred**, it is the first task (section 5.6) |
| ADR-088 | §4.2 and §9.1: *"no TypeScript runner for now"* | A TypeScript corpus runner **is** needed (for the WASM binding). The **TS SDK for outside developers stays deferred** (the architecture owner). The internal TS typed surface is not a published SDK |
| ADR-088 | §8.4 task 3, *"Web binding: the Dioxus rungs consume the core as a Rust dependency"* | Replaced by tasks 1 to 7 of section 10 |
| ADR-089 | Banner and §1 (*"while the platform focuses on Dioxus"*); the "Owned fork of the Dioxus subset" row; islands (`<spot-island>` hydrated by *"the rung-2/3 bundle"*); render cache designed around a Rust renderer node | On hold anyway. When rungs 0 and 1 return, the render and cache layer is re-decided against an external-component renderer; the view-tree cache of ADR-084 §8 item 1 is renderer-neutral and unaffected |
| ADR-083 | §2 consumer surface table (*"A designed Dioxus page for a journey"*) and §Kratos paragraph (*"ours in Rust (the charter's Dioxus and MiniJinja stack)"*) | "Dioxus" becomes "React"; the Kratos objection "its screens would be ours, in Rust" no longer holds, which reopens the headless-identity UI question (not decided here; noted as a consequence) |
| Web runtime charter | Decision 6 (*"Rust only; no JavaScript ecosystem"*); §9 Dioxus notes; §8 question 8 (mobile stack) | Charter edit by the lead after acceptance |

## 4. Proposed defaults (for the architecture owner to confirm)

**Selection rule for every library not named below** (so that agents do not invent): a library is eligible if it is (a) the one the framework's own documentation recommends, or failing that the most widely used for the job by npm weekly downloads; (b) released within the last 6 months and with a maintainer response to an issue within 30 days; (c) with a permissive licence already on `deny.toml`'s list or added to it by the lead; (d) with zero or few runtime dependencies; and (e) not a second way to do something the Rust core already does (section 4.2). The first task of section 10 that needs a library records the numbers it used. **Names in the "candidate" column are ASSUMED until that task verifies them.**

| Concern | Proposed default | Reason | Tag |
| --- | --- | --- | --- |
| Language | TypeScript, strict mode | The generated contract surface and the corpus runner are typed; agents write it well (judgement) | ASSUMED |
| Web rendering model | Browser-only React app first; **Next.js** as the SSR/SEO option, running only as the contained external component | Rungs 0 and 1 are on hold, so there is no SSR requirement; React Router documents SPA, SSR and static strategies as well, so the choice is open (question 1) | Next.js: VERIFIED that it self-hosts on Node with a reverse proxy recommended and needs shared-cache and encryption-key configuration for multiple instances (section 9); React Router modes VERIFIED |
| Avoid React Server Components by default | Yes, until a requirement appears | The critical unauthenticated RCE of CVE-2025-55182 lives in the server components packages; apps without server-component support are not affected (VERIFIED) | VERIFIED |
| Mobile | **Expo** (managed workflow with development builds and prebuild), React Native 0.88 line as the starting pin | Expo's Continuous Native Generation regenerates `android` and `ios` from config and config plugins; Expo Go cannot load arbitrary native code so a **development build** is required for our native core module (VERIFIED, section 5.3) | VERIFIED; RN 0.88 released 19 Oct 2026, New Architecture only since 0.82, Hermes V1 default since 0.84 (VERIFIED) |
| Workspace and monorepo | **pnpm** workspaces in the existing monorepo under a new top-level directory (name chosen by the web-shell task) | pnpm documents `allowBuilds` (build scripts off unless listed), `strictDepBuilds`, `minimumReleaseAge` (default 1,440 minutes), `trustPolicy` and `blockExoticSubdeps` (default on) (VERIFIED) | VERIFIED that the settings exist; that pnpm is the most used is ASSUMED |
| Router | The one the chosen web model ships with (Next.js router, or React Router); on mobile the router Expo documents (Expo Router, ASSUMED) | Rule (a) | ASSUMED |
| Server state, cache, queue | **None**: the Rust core owns it (section 4.2) | ADR-088 | n/a |
| UI state | React's own state and context; one small external-store library only if a measured need appears | Rule (e) and "no custom framework" | ASSUMED |
| Forms | Server-evaluated form events through the core (charter 4.4, ADR-084); no form library by default | Validation is the core's, not the page's | n/a |
| Component library | **None**: the closed control catalogue of ADR-068/069 is rendered by our renderers (section 6) | Keeps the vocabulary closed | n/a |
| Styling | Design tokens from the compiled UI bundle (ADR-069) emitted as CSS custom properties (web) and a token object (mobile); CSS logical properties for right-to-left | Section 8.2 | ASSUMED |
| Build tool | The one the web model requires; for a browser-only app the bundler must load a wasm-bindgen module (section 5.2). **Which bundlers load wasm-bindgen output is a gate item**, because the wasm-bindgen deployment page lists webpack as the only fully compatible bundler, Rolldown through a plugin, and does not mention Vite or Next.js | VERIFIED for what the page says | VERIFIED / ASSUMED for the rest |

### 4.1 What the defaults deliberately avoid

No Redux-style global store for server data; no GraphQL client; no CSS-in-JS runtime; no component kit; no Babel plugins beyond the framework's own; no polyfill bundles; no React Server Components; no second router. Each is a place where agents would otherwise be asked to choose and where the supply chain grows.

### 4.2 Why no server-state library

TanStack Query is the obvious candidate for caching and mutation on the web (VERIFIED: "fetches, caches, synchronizes, and updates server state", dedupes requests, background refresh; the page read does not mention React Native, mutations or offline persistence). It is **not adopted by default** because the core already owns the read cache with version checks, the command queue with idempotency keys and `If-Match` bases, and the realtime resync state machine (ADR-088 §5.1). Two caches would disagree, and divergent client behaviour is exactly what the spike measured (ADR-088 §2.2). React reads the core through a subscription hook (`useSyncExternalStore`-shaped, section 5.2).

## 5. The hard part: integrating with the Rust core

### 5.1 Restating why option C still holds

ADR-088 chose C partly because the web UI was Rust. That argument is gone for the web (section 3.2). The remaining arguments are stronger, not weaker, now that there are three non-Rust runtimes:

- **Three implementations of queue, cache and resync would be three TypeScript/C# codebases** (web TS, mobile TS, desktop C#) instead of two, and the spike showed two independent implementations of one specification disagreeing on key generation and queue durability (MEASURED, ADR-088 §2.2). React and React Native could share one TypeScript implementation, which is option B's best case and would be a real alternative: one TS client library for web and mobile, C# for Avalonia, corpus for both. It is **recorded as the fallback** (section 11) if the React Native binding gate fails, and it is the honest strongest argument against C here: it removes the FFI from two of three platforms.
- C keeps one source of truth for security-relevant behaviour (CSPRNG idempotency keys, token only in memory, queue cleared on sign-out, `If-Match` bases, the one retry after re-authentication). A TypeScript implementation could do the same; the point is that it would be implemented twice.
- The core runs in `cargo test` with a fake transport and no browser; the 12 data-layer checks that need Playwright today become data (ADR-088 §5.4).

### 5.2 Web: the core compiled to WebAssembly, consumed from TypeScript

**Shape (proposed).** `spot-client-core` compiled to `wasm32-unknown-unknown` with wasm-bindgen; the generated JavaScript glue and the `.wasm` file are published as a workspace package; a thin TypeScript package wraps it into a typed object whose methods are generated from the contract (the generated typed surface of ADR-088 §5.7, for our own client only; **not** a published SDK, which the architecture owner deferred).

- **Boundary.** One entry, `step(input) -> output`, plus a few observation calls. `input` is a serialised event (user action, transport reply, timer tick, storage reply); `output` is serialised effects (requests to send, storage writes, state snapshot, timers). This is what "sans-IO" means in ADR-088 §5.1 made concrete. The format (JSON versus a binary form) is a gate item; money stays a decimal string (ADR-012).
- **Loading.** wasm-bindgen's documented targets are `bundler` (default, for bundlers; "webpack is the only bundler the guide describes as fully compatible", Rolldown through a plugin), `web` (an ES module loaded directly in the browser, no npm dependencies), `nodejs`, `deno`, and `module` (source-phase imports, supported only in ESBuild and Node 24) (VERIFIED, wasm-bindgen.github.io deployment page; the older rustwasm.github.io copy says the same minus the last two). The documentation does not mention Vite or Next.js. **Therefore the gate must prove loading in the chosen bundler**, both the dev server and the production build, and prove it under the CSP of ADR-086 (WebAssembly compilation needs a CSP source expression; which one, and whether Trusted Types or `strict-dynamic` affect it, is ASSUMED and a gate item).
- **Where it runs: a dedicated Worker (proposed).** Instantiating the module, holding the access token in memory, running the queue and doing IndexedDB access (the persistence adapter of ADR-088 §5.5) in a Web Worker keeps the main thread free for React and keeps the token out of the page's global scope. The UI thread talks to the core by message passing; React subscribes to snapshots. What this does **not** do: XSS on the main thread can still send the worker commands (ADR-088 §7.2 says no client-side design addresses that). Multiple tabs would each hold a core; sharing one queue across tabs needs a coordination primitive (a SharedWorker or Web Locks); this is ASSUMED to be workable and is open question 10.
- **IndexedDB, never `localStorage`**, per principal, cleared on sign-out; memory-only in shared-device mode (ADR-088 §7.3).
- **SSR implications.** The core never runs on the server for rendering. If SSR returns, the server renders **view trees** (ADR-084) fetched from the hub; the core is loaded only in the browser (a client-only import). Server-side use of the core would also break the "token never leaves the browser's memory" property. Per-principal responses are not cached anywhere in v1 (ADR-089 §3.3 rule), so an SSR page for a signed-in user needs a request-scoped credential; that is one more reason SSR stays on hold until needed.
- **Node in CI.** The corpus is also driven in CI by the `nodejs` target of the same module (build-time use of Node, ADR-088 Q2). That is the cheap, browser-free runner.
- **Generated TS typed surface.** Generated from `contracts/native-api/openapi.yaml` with the same gate pattern as `tools/contract-clients/check.sh` (a third generator beside progenitor and Kiota). It builds requests and hands them to the core; the core decides whether, when and with which headers they are sent.
- **Session rule unchanged.** Bearer in the core's memory, renewal by an `HttpOnly; Secure; SameSite=Strict; __Host-` cookie at the consumer renewal endpoint (which does not exist yet; ADR-088 §7.4), WebSocket `auth` as first message, SSE read with a `fetch` reader.

### 5.3 React Native: Hermes does not run WebAssembly, so the core needs a native binding

**The constraint.** The React Native engine is Hermes (V1 is the default since React Native 0.84; VERIFIED, reactnative.dev blog listing). **WebAssembly in Hermes is an open feature request**: the Hermes issue "WASM support within Hermes?" (#429, opened 4 Dec 2020, 75 comments, label enhancement) is listed as open, with a related open pull request #2268 on WASM floating-point conversions whose meaning I could not read (VERIFIED for the listing; the Hermes README and `doc/Features.md` pages I read say nothing about WebAssembly in either direction). I therefore treat "Hermes cannot run the web WASM artefact in React Native 0.88" as **ASSUMED with strong support**, and the gate (section 5.6) must test it. It means **the web artefact cannot be reused on mobile**; mobile needs the core as native code.

**How native code reaches React Native (VERIFIED, reactnative.dev "Turbo Native Modules").** A TurboModule is defined by a typed TypeScript or Flow spec; Codegen generates the native interfaces; the Android implementation is Java or Kotlin and the iOS implementation is Objective-C++; C++ is supported through a separate cross-platform guide (linked, not read). The guide targets the New Architecture. Expo adds the **Expo Modules API** (Swift and Kotlin; its overview says to use Turbo Modules if you need C++) (VERIFIED).

**How Expo fits (VERIFIED, docs.expo.dev).**
- Continuous Native Generation: `npx expo prebuild` generates the `android` and `ios` directories from the app config, the SDK template, autolinking and config plugins; direct edits to those directories can be lost on `prebuild --clean`; customisations belong in config plugins.
- **Development builds**: a build with `expo-dev-client`, "your own version of Expo Go", free to use any native libraries. You must regenerate and rebuild after installing a library that contains native code, changing app config, or upgrading the SDK. Expo Go loads arbitrary projects only if native code has JavaScript fallbacks.
- **Consequence:** our native core module cannot run in Expo Go; every developer and agent working on the mobile app needs a development build (a Rust cross-toolchain for iOS and Android targets, Xcode for iOS builds, which needs macOS; ASSUMED, not researched). Expo documents nothing about Rust (the CNG page said so); how a Rust static library enters prebuild (a local config plugin plus autolinking) is ASSUMED and is a gate item.

**The options for binding Rust to React Native.**

| Option | What it is | Maturity evidence | Tag |
| --- | --- | --- | --- |
| **uniffi-bindgen-react-native** (`ubrn`) | A bindings generator built on UniFFI. Generates TypeScript and JSI C++ to call Rust from TypeScript and back, plus a TurboModule that installs the bindings into a running app. Also targets Web (via wasm-bindgen, or a `cdylib` for `wasm32-unknown-unknown` called directly) and Node (N-API). Sync and async calls, callbacks from Rust to JavaScript, objects by reference, records by value, tagged-union enums. MPL-2.0. Funded by Filament, Mozilla and Livekit; listed users include a Matrix SDK React Native library and two iOS/Android apps. Releases section empty on the page read; plans a rename to `uniffi-bindgen-javascript` | The project's own documentation says: *"This project is still in early development, and should not yet be used in production."* The Mozilla Hacks announcement (4 Dec 2024) calls it "an early release". Neither page mentions Expo or version compatibility | VERIFIED (README, docs site, announcement) |
| **Hand-written C ABI + a small JSI/C++ TurboModule** | The core exports a C ABI (`step(bytes) -> bytes`, `free`); one cross-platform C++ TurboModule exposes it to JavaScript via the React Native C++ module path | Relies on React Native's documented C++ route (the guide exists; not read). Cost: we maintain per-ABI build glue, but the boundary is a handful of functions because the core is sans-IO | Route VERIFIED to exist; cost ASSUMED |
| **UniFFI first-party Swift/Kotlin bindings + an Expo module or TurboModule** per platform | UniFFI generates Kotlin and Swift (full support, VERIFIED, mozilla.github.io/uniffi-rs); each is wrapped by Expo Modules API (Swift and Kotlin) or a Turbo module | UniFFI states no stability guarantee; third-party bindings "may need older UniFFI versions" (VERIFIED). Two native wrappers to maintain | VERIFIED / ASSUMED |
| **Other Rust-for-React-Native projects** (a tool called Craby and two `react-native-rs` repositories surfaced in search) | Claimed C++-TurboModule integration and automatic TypeScript types | Only search-result snippets read; benchmark claims are the author's own, from an extreme throughput test | ASSUMED, not evaluated; **not a candidate until someone reads them** |
| **Run the web WASM inside a hidden WebView** | Reuse the web artefact on mobile | Not researched. Rejected on principle: it puts the token, queue and storage in a WebView, defeating the point of a native client (ASSUMED) | ASSUMED |

**The key observation: the binding is small because the boundary is narrow.** ADR-088's core exchanges requests, replies and snapshots; a binding has to move bytes and call `step`. That makes the choice of generator **low-stakes and reversible**: `ubrn` can be tried first (it already generates the TurboModule glue and the TS side) and replaced by the hand-written C-ABI module if it proves unfit, with the same corpus proving both. The risk is not capability; it is maintenance and production-readiness of the generator, and its own statement is that it should not yet be used in production. The Matrix SDK library listing is the only production-ish signal on the page, and it is a signal, not a guarantee (ASSUMED).

### 5.4 Desktop: Avalonia calls the same core through a native library

Unchanged from ADR-088 §5.9 and §8.3: a native library over a C ABI, subject to the gate. Two UniFFI-related facts (VERIFIED, github.com/NordSecurity/uniffi-bindgen-cs): a third-party C# generator exists, young (major version 0; "unclear how stable the generated bindings are going to be between versions"), each release targets one UniFFI version (v0.11 targets uniffi-rs 0.31.0), needs .NET 8 with `AllowUnsafeBlocks`, minimum Rust 1.88, strings, byte arrays and lists limited to 2^31 elements. Because the same narrow `step` boundary serves C#, the hand-written route (`DllImport` of the C ABI) is the equally valid alternative. This ADR does not choose between them; the gate does.

### 5.5 One conformance corpus drives all bindings

The corpus of ADR-088 §9 (queue, conflicts, auth, realtime, SSE, cache, money, limits; a case is a scripted session; "a case changes only after the specification changes first") is the single arbiter. It already runs from `cargo test`. This ADR adds three required runners: (1) TypeScript over the `nodejs` WASM target in CI, plus a browser run through Playwright; (2) React Native: the corpus runs inside a development build on an iOS simulator and an Android emulator, reporting results to CI (the device runner tool is chosen by the selection rule; not researched); (3) C# through the native library under xunit. The matrix is printed in the `tools/sdk-conformance/matrix.py` style: a binding that cannot run a case prints `BLOCKED(<step>, <reason>)`, never silence.

### 5.6 The updated evidence gate (proposed, replaces ADR-088 §8.3)

**First task (core lane):** build the skeleton core and corpus (ADR-088 task 1), then drive the identical corpus from **three** bindings, with these outputs recorded in a short written verdict:

| Leg | Pass condition | Also record |
| --- | --- | --- |
| **TS over WASM** (Node CI and a real browser) | Every case identical to the Rust runner; loads in the chosen bundler's dev and production builds under the CSP of ADR-086; runs in a Worker with IndexedDB persistence across a reload | gzip size of `.wasm` and glue (against the S1 and S3 budgets), cold init time, per-`step` call overhead, lines of glue |
| **React Native native module** (iOS simulator and Android emulator, Expo development build) | Every case identical; builds through `expo prebuild` with a local config plugin; **tried first with `ubrn`, then with the hand-written C ABI + C++ module**, so the verdict compares them | binary size per ABI, build time, cold start, call overhead, lines of glue, whether generated code survives `prebuild --clean`, what the New Architecture and Hermes V1 combination requires |
| **C# over a native library** (Linux and Windows) | Every case identical | glue lines, marshalling issues, the `uniffi-bindgen-cs` versus `DllImport` comparison |

**Gate outcomes.** (1) All three legs pass: the core is the plan for all platforms. (2) The web leg passes and the React Native leg fails on both routes: **mobile and web share one TypeScript implementation** held to the corpus (ADR-088 option B for those two platforms), the core remains for the hub-side tests and for Avalonia if its leg passes, and the lead reopens this section with the architecture owner. (3) The C# leg fails: Avalonia keeps its Kiota client held to the corpus (ADR-088 §5.9, unchanged). The gate is a task, not a spike: its code is the core's first real code.

## 6. The control-catalogue renderer in React and React Native

**Requirement (ADR-084 §9):** a node kind exists only when every renderer implements it; renderers are held to a corpus of trees with the expected accessibility tree and event mapping; the vocabulary is closed (depth 8, 500 nodes, no free markup).

**Proposal: one shared framework-neutral TypeScript package plus two thin renderers.**

1. **`viewtree` package (shared, no React, no DOM).** Types generated from the ADR-084 interface; validation of the bounds; the patch applier for `set-prop`, `replace`, `insert`, `remove`, `move`; the event mapping (node key and event to the value sent back to the host); the table that maps each node kind to its accessible role and required props. This is plain TypeScript and is tested by the corpus in Node.
2. **Web renderer.** A recursive function over the tree with a table `{ kind -> React component }` rendering DOM elements; focus and scroll stay on nodes whose key survives (React reconciliation by `key`); styling from tokens.
3. **React Native renderer.** The same table shape, mapping kinds to React Native primitives; accessibility props from the shared role table; the `widget` node renders its mandatory fallback subtree (ADR-084 §13: widgets are web-only).
4. **Not proposed:** a cross-platform UI abstraction layer, or writing one set of components and running it on both (react-native-web is the known way; not researched, recorded as an alternative in section 11). Dense staff data screens (grids, forms) and a native mobile list are different enough that forcing one component set is the kind of custom engineering the architecture owner ruled out. Two renderers over one validated tree keep each small; the corpus of trees keeps them aligned.

**Where the renderers are used.** First-party and tier-A screens (definitions compiled by the ADR-069 bundle compiler) and view trees from server-hosted components render through the same catalogue code. Avalonia keeps its own renderer (ADR-084 §9).

## 7. Supply chain

### 7.1 Why this is the section that costs the most

The architecture owner's npm lockfile resolution of 27 Oct 2026 (**MEASURED by the architecture owner, reproduced here, not re-run**). Counts are non-optional unique packages; they **exclude** optional per-platform binaries and the dev tooling of apps, so they understate:

| Stack | Unique non-optional packages |
| --- | --- |
| react | 3 |
| react-dom + react | 3 |
| Svelte | 19 |
| Vue | 23 |
| Angular | 30 |
| Next.js 16.4 + React | 15 (the framework bundles much of its dependency set) |
| Nuxt | 460 |
| React Native 0.88 | 188 |
| Expo 58 + React Native | 397 |
| Capacitor | 95 |

Read it as a snapshot of one day, not a property of the frameworks. The conclusion for us: **the web shell on React alone is a very small tree; Next.js adds a bundled one; the React Native shell is two orders of magnitude larger, and Expo roughly doubles it.** That asymmetry is where the supply-chain effort goes, and it is an argument for starting the web client without a meta-framework.

**Server-layer security record (a known risk).**
- React Server Components: CVE-2025-55182, **CVSS 10.0, unauthenticated remote code execution**, affecting `react-server-dom-webpack`, `-parcel` and `-turbopack` 19.0, 19.1.0, 19.1.1 and 19.2.0 (fixed 19.0.1, 19.1.2, 19.2.1); Next.js affected and patched per release line; apps without a server or without server-component support are not affected; follow-up advisories listed on the same page: CVE-2025-55184 and CVE-2025-67779 (denial of service, CVSS 7.5), CVE-2025-55183 (source code exposure, CVSS 5.3), CVE-2026-23864 (denial of service, CVSS 7.5) (VERIFIED, react.dev).
- Next.js middleware authorisation bypass CVE-2025-29927 (internal header `x-middleware-subrequest`), CVSS about 9.1, self-hosted deployments affected, fixed in 15.2.3, 14.2.25, 13.5.9 and 12.3.5 (ASSUMED: from search-result snippets of third-party advisories; the Vercel postmortem was not opened).
- Reading: the server layer of a meta-framework is a real attack surface with a critical record. That is why the component is contained (section 9) and why server components are off by default.

### 7.2 Standard practice (adopted as written, no invention)

1. **Lockfiles committed, installs frozen in CI** (`--frozen-lockfile`-style). CODEOWNERS on every lockfile and on `package.json` files, owner the lead.
2. **Install scripts off by default.** pnpm runs dependency build scripts only for packages listed in `allowBuilds`; `strictDepBuilds` fails the install on a disallowed build script (both default on or documented as such; VERIFIED, pnpm.io/supply-chain-security). The allow-list is reviewed, small, and in the repository.
3. **Update cooldown.** pnpm `minimumReleaseAge` defaults to 1,440 minutes (one day); the page documents 10,080 for one week (VERIFIED). **Proposed: 7 days**, with an explicit, reviewed override path for security fixes. (Search results describe npm and Yarn gaining similar settings; ASSUMED, not verified.) A cooldown does not catch a long-dwelling compromise (secondary claim; ASSUMED).
4. **No exotic sources**: `blockExoticSubdeps` (git or tarball URL transitive dependencies; default on, VERIFIED). `trustPolicy: no-downgrade` blocks a package whose provenance trust dropped versus earlier releases (VERIFIED that it exists; adoption is a question for the first task).
5. **Vulnerability scanning in CI.** OSV-Scanner runs in CI against the lockfiles (VERIFIED that it is an official interface to the OSV database usable in a CI/CD pipeline; the list of supported lockfiles was not on the page I read, so pnpm-lockfile support and whether it also reads `Cargo.lock` are ASSUMED and checked by the task). `npm audit signatures` verifies registry signatures and provenance attestations; provenance links a package to its source and build and **does not prove it is free of malicious code** (VERIFIED, docs.npmjs.com).
6. **SBOM** generated per release for each shipped artefact (web bundle, container image, mobile build) in a standard format (CycloneDX or SPDX; the tool is chosen by the selection rule; ASSUMED).
7. **Our own packages** (the WASM core, the generated typed surface) are published with provenance from CI only; never from a laptop.
8. **Licence check** in the same job, against the list in `deny.toml` extended by the lead.
9. **Mobile adds:** a separate review of native dependencies pulled in by Expo's autolinking (Gradle and CocoaPods trees are not in the npm lockfile counts above; their size is unmeasured), and pinned Rust toolchains for the iOS and Android core builds.

### 7.3 Architectural containment (the part that does not depend on tooling)

A compromised npm package in the web shell can do what the page can do, and **no more than the user's own token allows**: the browser app holds no secret except the user's short-lived bearer in the Worker; every state-changing request is checked by the hub (permissions, validation, idempotency, version). If SSR returns, the server component runs with **no database credentials, no hub-internal credentials and no egress except to the hub's public API** (section 9). This is the architectural half of the policy: tooling reduces the probability, containment bounds the damage. What it does not bound: XSS or a hostile package acting through the core while the token is live (ADR-088 §7.2 states this as unaddressable client-side); the mitigations are CSP, the third-party policy of ADR-086, short token lives and the hub's own checks.

## 8. Third-party UI, accessibility, right-to-left, testing

### 8.1 Third-party UI under this stack (consequences for ADR-086 and ADR-084)

1. **Route (b), server-hosted components, is unchanged and becomes the main route**: a signed WASM component computes a view tree; React and React Native render it. No third-party code runs in the browser or the app for (a) and (b).
2. **Route (c), sandboxed widgets, is unchanged**: a cross-origin `<iframe sandbox="allow-scripts">` without `allow-same-origin`, from a separate widget origin, with a closed `postMessage` bridge (ADR-084 §13, ADR-086 §2.3a). React renders the `widget` node as that iframe. React Native renders the fallback subtree; whether a React Native WebView-hosted widget is ever allowed is out of scope (it would be a new decision).
3. **Bring your own front end** stays outside our stack (the architecture owner, 26 Oct): a customer may use any framework against the published API.
4. **A React app is a single-page app**, and ADR-086 §0 item 4 already says a PCI-relevant checkout must be a server-rendered page and not a route inside a client application (quoting PCI SSC's single-page-application guidance). Under React this makes the checkout page the clearest case for rung 0 or 1 (on hold) or for a separately served page; ADR-086's statement stands and the hold must be revisited before payments ship. This is flagged for the architecture owner as a consequence (question 8).
5. **Trusted Types and CSP** (ADR-086 §3.3): React's DOM writes are ASSUMED to be compatible; the audit of ADR-086 §3.9 proves it on the React shell. The WebAssembly compile permission needed by the core is added to the CSP reference policies (a gate item).

### 8.2 Accessibility and right-to-left (Urdu) as requirements

1. **Accessibility.** The charter's default proposal (WCAG 2.2 AA; who audits is an open charter question) applies to both renderers. The accessible role, name and state of each node come from the catalogue control, not the component (ADR-084 §11); the shared role table (section 6) feeds DOM semantics on the web and the accessibility props on React Native. The corpus of trees carries the expected accessibility tree per node. Automated checks on web; a manual screen-reader pass on iOS and Android per release until a device-level tool is chosen (ASSUMED, not researched).
2. **Right to left.** `html` carries `lang` and `dir` (as SPK-15 already requires: `?lang=ur`). Web uses CSS logical properties; the layout primitives are start/end, not left/right. React Native: how it flips layout under right-to-left is **ASSUMED** to be handled by its layout direction support; the RN shell task must prove Urdu rendering (Nastaliq shaping and font choice included) on both OSes. Mixed-direction text (Urdu with Latin numerals and codes) is a corpus case. Server-side localisation and formatting stay ADR-073's.
3. **Both are acceptance criteria of the shells**, not follow-ups.

### 8.3 Testing

- The **SPK-15 Playwright nightly harness is framework-neutral and stays** (`.github/workflows/spk15-nightly.yml`, `docs/spikes/SPK-15/`, `lab/SPK-15/common/accept/`): it talks to a URL and uses `data-testid`, roles, accessible names and the `?lang=ur` switch. It becomes the web shell's browser gate, with the hydration-mismatch checks relevant only if SSR returns.
- Browser-free first: the corpus (Rust, Node/WASM, C#) and the corpus of trees (Node) carry most behaviour; Playwright is for what only a browser shows (loading the WASM, CSP, IndexedDB across reload, focus, scroll, visual direction).
- Mobile: the corpus inside a development build on simulators (section 5.6) and a minimal end-to-end sign-in and list screen per release; the device-level tool is chosen by the selection rule.

## 9. Deployment: the JavaScript runtime as an optional external component

**Relation to the parallel ADR.** The generic mechanics (an external service component is not installed by default, is listed in the Admin Center, is enabled on demand, scaled separately) belong to the ADR-090 "External service components". This section states only what is specific to the web client, and defers to ADR-090 where they differ.

1. **When a runtime exists at all.** A browser-only React app is static files: any static host, or the hub's existing HTTP layer, can serve them. **No JavaScript runtime is needed on the server.** A Node-class runtime exists only if SSR returns (rungs 0 and 1 unhold) or a component needs server rendering. This is the strongest practical reason to start browser-only.
2. **What the component is.** A container image holding the built web shell (and, if SSR, the Next.js server), reachable only through a reverse proxy (Next.js documents recommending one rather than exposing the server; it also documents multi-instance needs: a shared cache handler, the same Server Functions encryption key across instances, and a `deploymentId` for version-skew protection; VERIFIED, nextjs.org self-hosting guide, last updated 15 Oct 2026).
3. **Untrusted client.** The component is a client of the public APIs like any outside developer's front end (ADR-088 §5.1 "two kinds of client"). It has no database access, no hub-internal or signing credentials, no ability to bypass validation or permissions, and **egress only to the hub's public API** (and the CDN or widget origins it serves). The hub does not trust a request because it came from the component: it authenticates the user's token exactly as for any client.
4. **Per-audience instances.** One instance (or release) per audience: staff, partner, consumer (the consumer web being an edge of the same tenant, ADR-083). Separate origins, separate CSP registry entries (ADR-086), separately scaled and separately switched on in the Admin Center. A compromise of the consumer instance does not reach the staff origin.
5. **Versioning against the hub contract.** The shell pins the native API contract version it was generated against (ADR-074 schema pinning, `contracts/native-api`), sends it, and fails closed on a major mismatch; the hub keeps additive compatibility within a major. The generated TS surface carries the contract version it was built from. Rolling releases use a deployment identifier so old tabs and new servers do not mix assets.
6. **Edge.** A consumer edge runs the same hub binary (ADR-083 §2.7) and serves the same static bundle; where an edge is offline or small, a static bundle is a smaller footprint than a runtime (ASSUMED; unmeasured).
7. **Mobile apps** are not deployed by the component; they are store releases built with EAS or local builds, versioned against the same contract and checked at start-up (store review cycles mean the hub must keep older contract minors alive; the support window is question 5).

## 10. Proposed task slate (not registered)

The UI stage and the platform stage are open (ADR-060); nothing here touches Foundation or ERP. Order is the build order.

| # | Task | Lane | Depends | Output and acceptance |
| --- | --- | --- | --- | --- |
| 1 | **Core skeleton and corpus, Rust and Node/WASM runners** (ADR-088 task 1, first half): `crates/framework/spot-client-core`, the corpus, a `cargo test` runner, a `wasm-bindgen` build and a `nodejs`-target TS runner | core | ADR-088 accepted, this ADR accepted | Corpus passes in Rust and Node; WASM size recorded |
| 2 | **Binding gate, web leg**: browser run (Worker, IndexedDB across reload, CSP with WebAssembly, chosen bundler dev and production) | core | 1 | Section 5.6 row 1 filled; bundler choice recorded |
| 3 | **Binding gate, React Native leg**: Expo development build, `prebuild` with a local config plugin, `ubrn` first then hand-written C ABI + C++ module, iOS simulator and Android emulator | core | 1 | Section 5.6 row 2; the two routes compared |
| 4 | **Binding gate, C# leg**: native library, xunit runner, `uniffi-bindgen-cs` versus `DllImport` | core | 1 | Section 5.6 row 3 |
| 5 | **Gate verdict and ADR amendments**: the written verdict; the section 3.2 amendments to ADR-084, 086, 088, 089, 083 and the charter; closing SPK-09 as a mobile-stack choice | lead | 2, 3, 4 | Amendment PRs; the architecture owner reviews the verdict |
| 6 | **Web shell**: pnpm workspace, TypeScript, browser-only React app, the wasm core in a Worker, sign-in and renewal per ADR-088 §7.2, generated TS typed surface (with a `check.sh`-style gate), React gzip size measured against the SPK-15 budgets | ui | 5 | Shell builds in CI; SPK-15 harness smoke slice runs against it; sizes recorded |
| 7 | **Generated TS typed surface from the contract** (may precede 6): third generator beside progenitor and Kiota, exactness gate with a self-test | general | ADR-088 accepted | `tools/contract-clients/` extended; gate green |
| 8 | **Catalogue renderers**: shared `viewtree` package, web renderer, corpus-of-trees runner (accessibility tree and event mapping), accessibility and right-to-left cases | ui | 6, ADR-084 T1 to T3 | Corpus of trees passes on web and in Node |
| 9 | **React Native shell**: Expo app, development build, native core module (route chosen at the gate), sign-in with secure storage, one list and one form screen, Urdu right-to-left proof on both OSes | ui | 5, 8 | Builds on both OSes; corpus runs in-app |
| 10 | **React Native renderer** over the shared `viewtree` package | ui | 8, 9 | Corpus of trees passes on device |
| 11 | **Supply-chain gates**: pnpm settings file, CODEOWNERS on lockfiles, OSV scanning, `npm audit signatures`, SBOM, provenance publishing, licence check; written beside the vendoring check of ADR-086 | general | 6 | A mutated lockfile or a package with an install script fails CI |
| 12 | **External component packaging and Admin Center entry** for the web shell (static image first; Next.js image only if SSR is approved), per-audience instances, egress policy, contract-version pin | core | ADR-090 accepted, 6 | Image builds; listed and switched on in the Admin Center; hub rejects an unsupported contract major |

Later and not scheduled: SSR evaluation when the hold lifts (Next.js against React Router framework mode, on a measured public page); the white-label build pipeline if question 3 goes that way; device-level accessibility automation.

**Stage note:** these are platform and UI-stage work and need no stage opening. The consumer renewal endpoint (ADR-088 §11 follow-up) is a prerequisite of task 6's sign-in and is a lead contract task, separate.

## 11. Risks, what would change the decision, and alternatives considered

### 11.1 Risks

| Risk | Likelihood and effect | Handling |
| --- | --- | --- |
| The React Native binding does not meet production quality (the generator is "early development"; unproven under Expo, New Architecture and Hermes V1 together) | Medium; the mobile leg is the long pole | Gate (section 5.6); hand-written C ABI route as the second try; shared TS implementation as the fallback outcome |
| A JavaScript supply-chain incident reaches the shells | Medium over time; high impact on mobile (397 packages) | Section 7; containment (token short-lived, hub checks everything) |
| Server-layer vulnerabilities (RSC, Next.js) | Real record (section 7.1) | Browser-only first; RSC off; contained component; patch cadence in the Admin Center |
| WASM loading and CSP conflicts in the chosen bundler | Medium; discovered late | Gate row 1 before any shell work |
| React bundle weight exceeds the SPK-15 budgets once the core is added | Unknown; React's gzip size is unmeasured | Task 6 measures; the budgets are the architecture owner's to move, not ours |
| Two renderers drift | Medium | Closed vocabulary, corpus of trees run by both |
| Mobile build tooling cost for agents (Xcode needs macOS; simulators on the laptop and CI runners) | Medium; not researched | A task-3 finding; CI macOS runner availability is an open question |
| Store policies (review times, remote code, OS-version support) | Not researched | Question 3 and a later mobile charter |

### 11.2 What would change the decision

- Both React Native binding routes fail the gate: mobile and web share a TypeScript data layer (section 5.6 outcome 2).
- Hermes gains WebAssembly support in a release we can adopt: the web artefact is reused on mobile, the native module disappears (ASSUMED likely to be attractive; track Hermes issue #429).
- A second critical flaw in the contained component, or a supply-chain incident that the controls would not have stopped: the browser-only recommendation hardens into a rule.
- The React shell's measured weight cannot meet the budgets: either the architecture owner moves the budgets or a lighter renderer is considered.

### 11.3 Alternatives considered

1. **Keep Dioxus.** Rejected by the architecture owner (reasons in section 2.2).
2. **React Native Web: one set of React Native components for web and mobile.** Not researched. Attractive for sharing the renderer; risks forcing the staff web's dense screens into mobile primitives. Left for question 9.
3. **Flutter, Kotlin Multiplatform, Capacitor.** Not researched here. Capacitor's lockfile count (95, MEASURED by the architecture owner) is in the table for comparison only; the architecture owner chose React Native.
4. **TypeScript data layer for web and mobile, no Rust core there (ADR-088 option B for those two).** Section 5.1: the recorded fallback and the strongest argument against the binding work.
5. **Svelte, Vue or Angular for the web.** Smaller or comparable trees in the architecture owner's snapshot (19, 23, 30) but no mobile story sharing the stack and no sign the architecture owner wants them; not pursued.

## 12. Open questions for the architecture owner (with recommendations)

1. **Next.js, React Router framework mode, or a browser-only app?** Next.js is the proposed default if SSR is needed. Rungs 0 and 1 are on hold, so SSR is not needed. **Recommend: browser-only React with a router now** (no server runtime, smallest tree, no server-layer exposure), and an evaluation task of Next.js against React Router's framework mode (it documents SPA, SSR and static rendering through a Vite plugin; VERIFIED) when the hold lifts. Keep routes and data loading outside framework-specific APIs so the choice stays open.
2. **Expo or bare React Native?** **Recommend Expo with development builds and prebuild**: native directories are generated from config and plugins, which suits agents and CI; our native core module is a local config plugin. Cost: no Expo Go; Expo's own native dependency tree (397 packages with React Native in the snapshot).
3. **One app, or white-label apps per tenant?** **Recommend one app per audience, tenant chosen at sign-in (a hub address or a tenant code), tenant branding delivered as data** (brand tokens, ADR-069 layers), because white-label builds multiply store accounts, review cycles and build pipelines. A white-label build pipeline can come later as an option. Not researched: store policy on apps that load per-tenant configuration.
4. **Which audience is the first mobile client: staff, consumer or both?** Not decided. Recommend consumer first (the charter's marketplace and citizen-service workloads), staff staying on Avalonia desktop plus a lighter mobile view later.
5. **Mobile support window against the hub contract.** Store review makes old app versions live for months. Recommend the hub keep the previous contract minor available for a stated period and the app show an upgrade notice (ADR-084's renderer rule) rather than break.
6. **Is a staff web client required at all?** Avalonia is the staff client (desktop), and a staff web UI doubles the staff surface. Recommend: not in the first slate; the web shell serves consumer and partner audiences first. The architecture owner decides.
7. **Amend charter decision 6.** Recommend recording: JavaScript and TypeScript are allowed in the client layer; the hub stays free of a server-side JavaScript runtime; any Node-class runtime is the optional external component; Node as a build and CI tool is allowed (ADR-088 Q2).
8. **Payments and the single-page app.** ADR-086 requires a PCI-relevant checkout to be a server-rendered page, not a route inside a client app. Recommend that the hold on rungs 0 and 1 is reviewed before any payment feature, with the checkout page as the first rung-0/1 page; or a separately served minimal checkout document.
9. **React Native Web (one component set) versus two renderers.** Recommend two renderers over one validated tree (section 6) until a measured reason appears.
10. **Multi-tab behaviour of the browser core** (one queue shared across tabs via a SharedWorker or Web Locks, or one core per tab with the queue owned by a leader). Recommend a leader election with a documented limit, decided by the web gate leg; the choice affects the persistence design in ADR-088 §5.5.
11. **A component or data library beyond section 4?** Default is none. Any request goes through the selection rule and the lead.
12. **Accessibility target and auditor** (the charter's open question 6) apply unchanged; recommend WCAG 2.2 AA and the auditor named before the first public consumer page ships.

## 13. Sources, and what was not verified

### 13.1 Read on 27 Oct 2026 (VERIFIED for what the page says)

- github.com/jhugman/uniffi-bindgen-react-native (README): generates TS bindings and JSI C++ with a TurboModule; WASM and Node targets; MPL-2.0; Releases empty; planned rename to `uniffi-bindgen-javascript`; funders; users. Expo and Hermes not mentioned.
- jhugman.github.io/uniffi-bindgen-react-native (docs): *"still in early development, and should not yet be used in production"*; WASM through wasm-bindgen or a direct `cdylib`; Hermes via JSI.
- hacks.mozilla.org/2024/12/introducing-uniffi-for-react-native-rust-powered-turbo-modules/ (4 Dec 2024): "an early release"; Mozilla and Filament.
- mozilla.github.io/uniffi-rs/latest/: Kotlin, Swift, Python full support; Ruby partial; third-party bindings may need older UniFFI versions.
- github.com/NordSecurity/uniffi-bindgen-cs: version mapping, young, .NET and Rust requirements, limits.
- github.com/facebook/hermes issues listing (query "WebAssembly"): issue #429 open since Dec 2020; PR #2268 open. README and `doc/Features.md`: no statement on WebAssembly; supported and excluded ES features.
- reactnative.dev/blog: React Native 0.88 (19 Oct 2026); New Architecture by default (0.76), first fully New Architecture (0.82), Hermes V1 default (0.84).
- reactnative.dev/docs/turbo-native-modules-introduction: spec, Codegen, Java/Kotlin, Objective-C++, C++ guide link.
- docs.expo.dev: continuous-native-generation (prebuild, config plugins, Expo Go exception); development-builds introduction; modules/overview (Expo Modules API in Swift and Kotlin; Turbo Modules for C++).
- wasm-bindgen.github.io/wasm-bindgen/reference/deployment.html and rustwasm.github.io (older copy): deployment targets and bundler notes.
- nextjs.org/docs/app/guides/self-hosting (lastUpdated 2026-10-15): reverse proxy, shared cache handler, encryption key, deployment ID.
- react.dev/blog/2025/12/03/critical-security-vulnerability-in-react-server-components: CVE-2025-55182 and follow-ups.
- reactrouter.com/start/modes: declarative, data and framework modes; framework mode supports SPA, SSR and static rendering.
- tanstack.com/query/latest/docs/framework/react/overview: what it is.
- pnpm.io/supply-chain-security: `allowBuilds`, `strictDepBuilds`, `minimumReleaseAge`, `trustPolicy`, `blockExoticSubdeps`.
- docs.npmjs.com/generating-provenance-statements: provenance and `npm audit signatures`.
- google.github.io/osv-scanner: official OSV interface, CI usable.

### 13.2 ASSUMED (search snippets, pages that did not say it, or general knowledge)

CVE-2025-29927 details (search results from Fastly, UpGuard, Qualys and others; the primary Vercel postmortem was not opened); npm and Yarn release-age settings and the Shai-Hulud background (Snyk and blog snippets); the Craby and `react-native-rs` projects (snippets only); that React writes the DOM without `innerHTML`; that Hermes in 0.88 cannot instantiate WebAssembly (the open-issue evidence is strong, the shipping behaviour was not tested); that Vite or Next.js can load wasm-bindgen output (not in the page); the Worker, IndexedDB and SharedWorker design; React Native layout-direction handling; Expo Router; macOS requirement for iOS builds; OSV-Scanner lockfile coverage; which SBOM format and which device-test tool.

### 13.3 MEASURED (from this repository or from the architecture owner, reproduced)

SPK-15 page weights, check counts and fork sizing (via ADR-088 §2.1 and `docs/spikes/SPK-15/`); the architecture owner's lockfile snapshot of 27 Oct 2026 (section 7.1), not re-run by me.

### 13.4 Not researched at all

Flutter, Kotlin Multiplatform, Capacitor beyond the one lockfile number; react-native-web; Expo SDK 58 specifics and whether `ubrn` works under it; store policies; EAS pricing and limits; the size and Core Web Vitals of React plus the core; Playwright alternatives for mobile; Urdu typography on React Native; the wasm-bindgen output in Next.js's two bundlers.
