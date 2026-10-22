//! The destructive-command gate for the `bash` tool (issue #604).
//!
//! Kept in its own file so the policy seam is easy to find and review: this is
//! the only thing standing between a model's `rm -rf` and the user's data.

/// Apply the deterministic destructive-command gate, returning refusal text
/// when the command must not run as-issued.
///
/// Stage 1 is a pure blast-radius assessment; stage 2 turns a `Confirm` verdict
/// into a reflection prompt that a blind retry cannot satisfy. Catastrophic
/// targets (`/`, `$HOME`, credential stores, device nodes) are denied outright.
/// See issue #604.
pub(super) fn destructive_command_refusal(
    command: &str,
    justification: Option<&str>,
    working_dir: Option<std::path::PathBuf>,
) -> Option<String> {
    let mut risk_ctx = jcode_command_risk::RiskContext::from_env(working_dir);
    // Assess the same scratch path that the child shell actually receives.
    #[cfg(not(windows))]
    {
        risk_ctx.scratch_dir = super::tool_scratch_dir();
    }
    let assessment = jcode_command_risk::assess(command, &risk_ctx);
    if assessment.level.runs_immediately() {
        return None;
    }

    let justification = jcode_command_risk::Justification {
        text: justification.map(str::to_string),
    };
    match jcode_command_risk::gate(&assessment, &justification) {
        jcode_command_risk::GateOutcome::Allow => None,
        jcode_command_risk::GateOutcome::Deny { reason } => {
            crate::logging::warn(&format!("[bash] denied destructive command: {command}"));
            Some(reason)
        }
        jcode_command_risk::GateOutcome::Reflect { prompt } => {
            crate::logging::info(&format!(
                "[bash] destructive command held for justification: {command}"
            ));
            Some(prompt)
        }
    }
}

pub(super) fn cowork_command_requires_approval(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .filter(|token| !token.is_empty())
        .collect();
    if tokens
        .iter()
        .any(|token| matches!(*token, "remove-item" | "del" | "rmdir" | "format-volume"))
    {
        return true;
    }

    tokens.windows(3).any(|triple| {
        triple[0] == "invoke-webrequest"
            && triple[1] == "method"
            && matches!(triple[2], "post" | "put")
    })
}

/// The `bash` tool's JSON schema, including the `justification` field the
/// destructive-command gate consumes.
///
/// Lives beside the gate so the schema and the policy that reads it stay in
/// sync, and so bash.rs stays inside the code-size budget.
pub(super) fn bash_parameters_schema() -> serde_json::Value {
    let cmd_desc = if cfg!(windows) {
        "The Windows command to execute via cmd.exe. Use cmd.exe syntax and quoting, not Bash syntax."
    } else {
        "The bash command to execute. Put large temp files under `$JCODE_SCRATCH_DIR`, not `/tmp`."
    };
    serde_json::json!({
        "type": "object",
        "required": ["command"],
        "properties": {
            "intent": crate::tool::intent_schema_property(),
            "command": {
                "type": "string",
                "description": cmd_desc
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout in MILLISECONDS (not seconds), e.g. 600000 = 10min; kills with exit 124. Omit for no timeout."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Run in background. Emit `JCODE_PROGRESS {json}` lines for progress reporting."
            },
            "notify": {
                "type": "boolean",
                "description": "Notify on completion."
            },
            "wake": {
                "type": "boolean",
                "description": "Wake on completion."
            },
            "stall_wake_seconds": {
                "type": "integer",
                "description": "With run_in_background: wake after N seconds without output/progress (min 30). For jobs that may hang."
            },
            "justification": {
                "type": "string",
                "description": "Only when re-issuing a command the destructive gate refused; explain which user request it serves."
            }
        }
    })
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::destructive_command_refusal;

    #[test]
    fn scratch_log_and_backup_commands_do_not_require_justification() {
        let cwd = std::env::current_dir().ok();
        for command in [
            "cargo test --lib > \"$JCODE_SCRATCH_DIR/tests.log\" 2>&1",
            "git diff > \"${JCODE_SCRATCH_DIR}/before.patch\"",
            "env | grep JCODE",
            "command -v sudo && sudo -n true",
            "find /sys -type l -exec readlink {} \\;",
            "find /etc -type f -exec sed -n '1,10p' {} \\;",
        ] {
            assert!(
                destructive_command_refusal(command, None, cwd.clone()).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn protected_writes_and_unknown_variables_remain_blocked() {
        for command in [
            "rm -rf /etc",
            "echo bad > /etc/passwd",
            "find /etc -type f -exec rm {} \\;",
            "echo test > \"$UNKNOWN/tests.log\"",
        ] {
            assert!(
                destructive_command_refusal(command, None, std::env::current_dir().ok()).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn cowork_gate_flags_destructive_windows_commands_and_post_put_requests() {
        for command in [
            "Remove-Item report.docx",
            "del report.docx",
            "rmdir old-folder",
            "Format-Volume -DriveLetter E",
            "Invoke-WebRequest https://example.invalid -Method POST",
            "Invoke-WebRequest -Method=PUT https://example.invalid/api",
        ] {
            assert!(
                super::cowork_command_requires_approval(command),
                "{command}"
            );
        }

        for command in [
            "Get-Content report.docx",
            "Invoke-WebRequest https://example.invalid -Method GET",
        ] {
            assert!(
                !super::cowork_command_requires_approval(command),
                "{command}"
            );
        }
    }
}
