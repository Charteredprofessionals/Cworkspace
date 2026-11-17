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

/// True when a Cowork-session shell command must pause for user approval.
///
/// Two layers, both required:
/// 1. A Cowork denylist for Windows/Unix primitives the blast-radius engine
///    may not see through (PowerShell aliases, `cmd /c del`,
///    `-EncodedCommand` base64 wrapping, `reg delete`, `net user`).
/// 2. The same `jcode-command-risk` engine the CLI gate uses, so every
///    `Confirm`-or-worse verdict - including Unix `rm`/`dd`/`mkfs` and
///    cross-platform obfuscation - also pauses for approval in Cowork mode.
///
/// Catastrophic verdicts are already denied outright by
/// [`destructive_command_refusal`] inside the bash tool, so this function only
/// ever *adds* approval prompts on top of that refusal path.
pub(super) fn cowork_command_requires_approval(command: &str) -> bool {
    if cowork_denylist_matches(command) {
        return true;
    }

    let mut risk_ctx = jcode_command_risk::RiskContext::from_env(None);
    #[cfg(not(windows))]
    {
        risk_ctx.scratch_dir = super::tool_scratch_dir();
    }
    !jcode_command_risk::assess(command, &risk_ctx)
        .level
        .runs_immediately()
}

/// The cheap tokenized denylist. Kept separate from the risk-engine check so
/// the Cowork-specific Windows coverage stays testable in isolation.
fn cowork_denylist_matches(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    // Base64-wrapped PowerShell evades tokenization entirely; any encoded
    // payload must go through approval.
    if lower.contains("encodedcommand") {
        return true;
    }

    let tokens: Vec<&str> = lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .filter(|token| !token.is_empty())
        .collect();

    // Destructive primitives: PowerShell cmdlets and their aliases.
    if tokens.iter().any(|token| {
        matches!(
            *token,
            "remove-item"
                | "del"
                | "rmdir"
                | "rd"
                | "ri"
                | "erase"
                | "mi"
                | "format-volume"
                | "diskpart"
        )
    }) {
        return true;
    }

    // Windows delegation wrappers: cmd /c del, cmd /c rd.
    if tokens.windows(3).any(|triple| {
        triple[0] == "cmd" && triple[1] == "c" && matches!(triple[2], "del" | "rd" | "rmdir")
    }) {
        return true;
    }

    // Registry and account management.
    if tokens.windows(2).any(|pair| {
        (pair[0] == "reg" && pair[1] == "delete")
            || (pair[0] == "net" && matches!(pair[1], "user" | "localgroup"))
    }) {
        return true;
    }

    // Network writes across the common clients.
    if tokens
        .iter()
        .any(|token| matches!(*token, "curl" | "wget" | "invoke-webrequest" | "invoke-restmethod"))
        && tokens
            .iter()
            .any(|token| matches!(*token, "post" | "put" | "delete"))
    {
        return true;
    }

    // Unix destructive primitives (Cowork over the WS bridge runs these too).
    if tokens
        .iter()
        .any(|token| matches!(*token, "rm" | "shred" | "dd" | "mkfs" | "fdisk"))
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
            "ri report.docx",
            "rd old-folder",
            "erase report.docx",
            "Format-Volume -DriveLetter E",
            "Invoke-WebRequest https://example.invalid -Method POST",
            "Invoke-WebRequest -Method=PUT https://example.invalid/api",
            "cmd /c del report.docx",
            "reg delete HKLM\\Software\\Key",
            "net user attacker Password1! /add",
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

    #[test]
    fn cowork_gate_flags_encoded_payloads_and_unix_primitives() {
        for command in [
            // -EncodedCommand evades tokenization; always approve first.
            "powershell -EncodedCommand RwBvAG4AdA==",
            "pwsh -NoProfile -EncodedCommand AA==",
            "rm report.docx",
            "rm -rf build/",
            "shred secret.txt",
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sdb1",
            "curl -X POST https://example.invalid/upload",
        ] {
            assert!(
                super::cowork_command_requires_approval(command),
                "{command}"
            );
        }
    }

    #[test]
    fn cowork_gate_stays_quiet_for_ordinary_cowork_work() {
        for command in [
            "cargo test --lib",
            "git status --short",
            "python analyze.py --input data.csv",
            "ls reports",
            "echo done",
            "informational notes about rm tokens inside words", // 'rm' inside a word must not match
        ] {
            assert!(
                !super::cowork_command_requires_approval(command),
                "{command}"
            );
        }
    }
}
