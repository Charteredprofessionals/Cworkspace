//! Tests for the Cowork denylist escalation (S-3), ported from the hardened
//! `bash_destructive_gate.rs` test surface of the finalization audit and
//! narrowed to the Windows surface the blast-radius engine cannot see.

use super::{assess, RiskContext, RiskLevel};

fn level(command: &str) -> RiskLevel {
    assess(command, &RiskContext::from_env(None)).level
}

#[test]
fn cowork_gate_flags_destructive_windows_commands() {
    // PowerShell primitives and aliases.
    for command in [
        "Remove-Item C:\\tmp\\build -Recurse",
        "del results.csv",
        "ri HKLM:\\Software\\Vendor",
        "erase backup.old",
        "mi Default",
        "Format-Volume -DriveLetter D",
        "diskpart /s script.txt",
        "rmdir /s /q build",
    ] {
        assert!(
            !level(command).runs_immediately(),
            "flagged: {command}"
        );
    }
}

#[test]
fn cowork_gate_flags_encoded_payloads_and_windows_delegation() {
    // -EncodedCommand evades tokenization entirely: the blast-radius engine
    // sees nothing, so only the denylist escalates it to Confirm.
    assert_eq!(
        level("powershell -EncodedCommand RwBvAG4AdA=="),
        RiskLevel::Confirm
    );
    assert_eq!(
        level("pwsh -NoProfile -EncodedCommand AA=="),
        RiskLevel::Confirm
    );

    // Windows delegation wrappers and registry/account management.
    for command in [
        "cmd /c del logs\\*.tmp",
        "cmd /c rd /s /q out",
        "reg delete HKCU\\Temp /f",
        "net user attacker Password1! /add",
        "net localgroup admins attacker /add",
    ] {
        assert!(
            !level(command).runs_immediately(),
            "flagged: {command}"
        );
    }

    // Network writes through the PowerShell clients.
    assert!(
        !level("Invoke-WebRequest -Uri https://x.test -Method Post").runs_immediately()
    );
    assert!(
        !level("Invoke-RestMethod -Uri https://x.test -Method Put").runs_immediately()
    );
}

#[test]
fn cowork_gate_stays_quiet_for_ordinary_cowork_work() {
    for command in [
        "Get-ChildItem C:\\tmp",
        "Get-Content app.log",
        "git status --short",
        "curl -fsSL https://api.example.com/status",
        "node scripts/render-report.js --dry-run",
        "cat README.md",
    ] {
        assert!(
            level(command).runs_immediately(),
            "quiet: {command}"
        );
    }
}

#[test]
fn unix_destructive_primitives_stay_with_the_blast_radius_engine() {
    // The denylist deliberately does NOT name Unix primitives: the engine
    // classifies them by blast radius (its own tests pin this behavior).
    // Coverage is delegated, not lost: home-rooted rm is catastrophic and
    // dd to a raw device does not run immediately.
    assert_eq!(level("rm -rf ~"), RiskLevel::Catastrophic);
    assert!(!level("dd if=/dev/zero of=/dev/sda").runs_immediately());
    // Routine cleanup inside the project stays immediate by design.
    assert!(level("rm -rf target").runs_immediately());
}
