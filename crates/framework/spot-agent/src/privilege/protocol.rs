//! The Windows privileged helper protocol: a LocalSystem service accepts named operations
//! over a local named pipe whose ACL admits only the agent account (the service host is
//! WP-ADM-05.T6). This module holds the pure parts: the request/reply framing, the pipe
//! SDDL and the Windows template table, all testable on Linux.

use serde::{Deserialize, Serialize};

use super::templates::{Kind, Template, Tok};

/// The pipe name.
pub const PIPE_NAME: &str = r"\\.\pipe\SpotledgerAgentPriv";

/// One request line: an operation name and its parameters.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PipeRequest {
    /// The template name.
    pub op: String,
    /// The parameters.
    pub params: Vec<String>,
}

const SC: &str = r"C:\Windows\System32\sc.exe";
const SVC: Tok = Tok::Param(Kind::ServiceName);

/// The Windows privileged templates.
pub const WINDOWS_TEMPLATES: &[Template] = &[
    Template {
        name: "service.start",
        program: SC,
        tokens: &[Tok::Lit("start"), SVC],
        timeout_secs: 60,
        output_cap: 65_536,
        confirmation: false,
    },
    Template {
        name: "service.stop",
        program: SC,
        tokens: &[Tok::Lit("stop"), SVC],
        timeout_secs: 60,
        output_cap: 65_536,
        confirmation: true,
    },
    Template {
        name: "service.status",
        program: SC,
        tokens: &[Tok::Lit("query"), SVC],
        timeout_secs: 30,
        output_cap: 65_536,
        confirmation: false,
    },
];

/// Builds the pipe's SDDL: a protected DACL that denies network logons, grants the agent
/// account (`agent_sid`) read/write on the pipe and SYSTEM full control, nobody else.
///
/// # Errors
/// `Err(())` when `agent_sid` is not a well-formed SID.
#[allow(clippy::result_unit_err)]
pub fn pipe_sddl(agent_sid: &str) -> Result<String, ()> {
    let well_formed = agent_sid.starts_with("S-1-")
        && agent_sid.len() <= 184
        && agent_sid
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == 'S')
        && agent_sid.matches('-').count() >= 3;
    if !well_formed {
        return Err(());
    }
    Ok(format!(
        "D:P(D;;GA;;;NU)(A;;GRGW;;;{agent_sid})(A;;GA;;;SY)"
    ))
}

/// Decodes and validates one request line against [`WINDOWS_TEMPLATES`], returning the
/// program and argv the service would run.
///
/// # Errors
/// The refusal code.
pub fn decode_request(line: &str) -> Result<(String, Vec<String>), &'static str> {
    let request: PipeRequest =
        serde_json::from_str(line.trim()).map_err(|_| "agent.privilege_param_count")?;
    let template = WINDOWS_TEMPLATES
        .iter()
        .find(|t| t.name == request.op)
        .ok_or("agent.privilege_unknown_operation")?;
    let argv = template.build(&request.params).map_err(|r| r.code())?;
    Ok((template.program.to_owned(), argv))
}
