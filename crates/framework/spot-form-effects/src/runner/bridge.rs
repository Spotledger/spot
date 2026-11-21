//! The bridge a form script sees and the small JavaScript harness that gives it the
//! `ctx.effects` builder and the `(doc, changed, ctx)` signature.

use super::*;

/// The closed host surface: emit one effect and log one line.
pub(super) fn host_surface() -> HostSurface {
    HostSurface::new(vec![
        HostFn::new(HOST_EMIT, Shape::Any),
        HostFn::new(
            HOST_LOG,
            Shape::Object {
                required: vec![("message".to_owned(), Shape::Text)],
                open: true,
            },
        ),
    ])
}

/// The bridge: collects a script's effects and log lines, enforcing the caps as it goes so a
/// runaway script cannot even allocate an unbounded batch.
pub(super) struct EffectBridge {
    effects: Mutex<Vec<Json>>,
    logs: Mutex<Vec<String>>,
    /// The redaction layer every log line passes through; `None` means it could not be built
    /// and every line becomes a fixed marker (fail closed).
    redactor: Option<Arc<spot_redact::Redactor>>,
    /// How many more log lines this run may write before the per-event cap is reached.
    log_budget: usize,
    log_overflow: AtomicBool,
    limits: EffectLimits,
    bytes: AtomicUsize,
    overflow: AtomicBool,
    now_millis: i64,
    seed: u64,
}

impl EffectBridge {
    pub(super) fn new(
        limits: EffectLimits,
        redactor: Option<Arc<spot_redact::Redactor>>,
        log_budget: usize,
        now_millis: i64,
        seed: u64,
    ) -> EffectBridge {
        EffectBridge {
            effects: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
            redactor,
            log_budget,
            log_overflow: AtomicBool::new(false),
            limits,
            bytes: AtomicUsize::new(0),
            overflow: AtomicBool::new(false),
            now_millis,
            seed,
        }
    }

    /// Whether a log line was dropped because the per-event cap was reached.
    pub(super) fn log_overflowed(&self) -> bool {
        self.log_overflow.load(Ordering::Acquire)
    }

    pub(super) fn overflowed(&self) -> bool {
        self.overflow.load(Ordering::Acquire)
    }

    pub(super) fn take_effects(&self) -> Vec<Json> {
        std::mem::take(
            &mut *self
                .effects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub(super) fn take_logs(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn emit(&self, effect: Json) -> Result<Json, spot_ts::HostError> {
        let size = serde_json::to_vec(&effect).map_or(usize::MAX, |bytes| bytes.len());
        let count = self
            .effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let total = self.bytes.load(Ordering::Relaxed);
        if count >= self.limits.max_effects
            || size > self.limits.max_effect_bytes
            || total.saturating_add(size) > self.limits.max_batch_bytes
        {
            self.overflow.store(true, Ordering::Release);
            return Err(spot_ts::HostError::new(
                "limit",
                "the effect batch reached its limit",
            ));
        }
        self.bytes.fetch_add(size, Ordering::Relaxed);
        self.effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(effect);
        Ok(Json::Null)
    }
}

impl HostBridge for EffectBridge {
    fn call(&self, name: &str, args: Json) -> Result<Json, spot_ts::HostError> {
        match name {
            HOST_EMIT => self.emit(args),
            HOST_LOG => {
                let message = args
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                // Redact first, then cap: cutting first could leave half of a
                // secret that no rule recognises.
                let message = super::redaction::redact_line(self.redactor.as_deref(), message);
                let message = truncate_line(&message, self.limits.max_log_line_bytes);
                let mut logs = self
                    .logs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if logs.len() >= self.log_budget {
                    // Dropped, not an error: a chatty script is not a failing one.
                    self.log_overflow.store(true, Ordering::Release);
                } else {
                    logs.push(message);
                }
                Ok(Json::Null)
            }
            other => Err(spot_ts::HostError::new(
                "internal",
                &format!("`{other}` is not a form-script host function"),
            )),
        }
    }

    fn now_millis(&self) -> i64 {
        self.now_millis
    }

    fn random_seed(&self) -> u64 {
        self.seed
    }
}

/// The JavaScript appended to every transpiled script: the `ctx.effects` builder and the entry
/// wrapper that gives the author the signature the brief fixes, `(doc, changed, ctx)`.
pub(super) fn harness(entry: &str) -> String {
    format!(
        r#"
const __spot_effect = (effect) => {{ host.emit(effect); }};
const __spot_effects = Object.freeze({{
  setValue(path, value) {{ __spot_effect({{ kind: "set_value", path: path, value: value === undefined ? null : value }}); }},
  setReadOnly(path, value) {{ __spot_effect({{ kind: "set_read_only", path: path, read_only: !!value }}); }},
  setHidden(path, value) {{ __spot_effect({{ kind: "set_hidden", path: path, hidden: !!value }}); }},
  setRequired(path, value) {{ __spot_effect({{ kind: "set_required", path: path, required: !!value }}); }},
  setOptions(path, options) {{ __spot_effect({{ kind: "set_options", path: path, options: options === undefined ? [] : options }}); }},
  setLabel(path, label) {{ __spot_effect({{ kind: "set_label", path: path, label: label }}); }},
  addAction(id, label, effect) {{ __spot_effect({{ kind: "add_action", id: id, label: label, effect: effect }}); }},
  removeAction(id) {{ __spot_effect({{ kind: "remove_action", id: id }}); }},
  showMessage(level, text, field) {{ __spot_effect({{ kind: "show_message", level: level, text: text, field: field === undefined ? null : field }}); }},
  showMessageKey(level, key, field) {{ __spot_effect({{ kind: "show_message", level: level, text_key: key, field: field === undefined ? null : field }}); }},
  openDialog(dialogId, params) {{ __spot_effect({{ kind: "open_dialog", dialog_id: dialogId, params: params === undefined ? {{}} : params }}); }},
  focus(path) {{ __spot_effect({{ kind: "focus", path: path }}); }},
  setFilter(linkPath, filter) {{ __spot_effect({{ kind: "set_filter", link_path: linkPath, filter: filter }}); }},
  batch(effects) {{ __spot_effect({{ kind: "batch", effects: effects }}); }},
}});
export function __spot_form_run(__args) {{
  const __handler = typeof {entry} === "function" ? {entry} : undefined;
  if (__handler === undefined) {{
    throw {{
      code: "form_effects.missing_handler",
      message: "The script does not export {entry}.",
    }};
  }}
  const __ctx = {{
    event: __args.event,
    doctype: __args.doctype,
    id: __args.id,
    now: __args.now,
    effects: __spot_effects,
  }};
  const __result = __handler(__args.doc, __args.changed, __ctx);
  return __result === undefined ? null : __result;
}}
"#
    )
}

/// The lifecycle record shape `ScriptLimits::engine_limits` expects.
pub(super) fn lifecycle_record(script: &FormScript) -> spot_server_scripts::ServerScript {
    spot_server_scripts::ServerScript {
        record_id: None,
        name: script.name.clone(),
        doctype: script.doctype.clone(),
        event: spot_server_scripts::ScriptEvent::Validate,
        source: script.source.clone(),
        compiled_hash: None,
        timeout_ms: script.timeout_ms,
        priority: 0,
        enabled: script.enabled,
        disabled_reason: None,
    }
}

/// The marker appended to a log line that was cut.
const LOG_CUT_MARKER: &str = "...[truncated]";

/// `line` cut to at most `max_bytes` bytes (on a character boundary) with a marker, or whole.
fn truncate_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = line[..end].to_owned();
    out.push_str(LOG_CUT_MARKER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary_and_marked() {
        let line = "é".repeat(2_000);
        let cut = truncate_line(&line, 1_024);
        assert!(cut.ends_with(LOG_CUT_MARKER));
        assert!(cut.len() <= 1_024 + LOG_CUT_MARKER.len());
        assert!(truncate_line("short", 1_024) == "short");
    }

    #[test]
    fn lines_past_the_budget_are_dropped_and_flagged_once() {
        let bridge = EffectBridge::new(
            EffectLimits::default(),
            spot_redact::Redactor::from_policy(&spot_redact::RedactionPolicy::default())
                .ok()
                .map(Arc::new),
            2,
            0,
            0,
        );
        for index in 0..5 {
            let args = json!({ "message": format!("line {index}") });
            assert!(bridge.call(HOST_LOG, args).is_ok());
        }
        assert!(bridge.log_overflowed());
        assert_eq!(bridge.take_logs(), vec!["line 0", "line 1"]);
    }
}
