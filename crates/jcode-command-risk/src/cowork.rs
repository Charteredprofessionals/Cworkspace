//! Cowork-specific command denylist (S-3).
//!
//! The blast-radius engine classifies Unix commands by what they destroy, and
//! its tests deliberately reject name-based matching for them (`rm -rf target`
//! inside the project must stay immediate; `shred` of one file is `Low`). The
//! gap the finalization audit flagged (S-3) lives on the other side: Cowork
//! executes through Windows shells where primitives slip past a Unix-focused
//! tokenizer entirely — PowerShell aliases (`ri`, `mi`, `erase`),
//! `cmd /c del` delegation, `-EncodedCommand` base64 wrapping, `reg delete`
//! and `net user`. This module keeps the cheap tokenized denylist from the
//! hardened `bash_destructive_gate.rs` for exactly that surface: any match
//! escalates the assessment to [`RiskLevel::Confirm`] so the command can
//! never run without justification. Unix destructive primitives stay with
//! the blast-radius engine, which already covers them by design.

/// The cheap tokenized denylist. Kept separate from the risk-engine check so
/// the Cowork-specific Windows coverage stays testable in isolation.
pub(crate) fn matches(command: &str) -> bool {
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

    // Network writes through the PowerShell clients the engine cannot see.
    if tokens
        .iter()
        .any(|token| matches!(*token, "invoke-webrequest" | "invoke-restmethod"))
        && tokens
            .iter()
            .any(|token| matches!(*token, "post" | "put" | "delete"))
    {
        return true;
    }

    false
}
