//! `ac-ctl doctor` (ADR-014): the findings for a host and profile, as a table or JSON, and the
//! forecast of what the recommended settings allow. Read-only; the facts come from the collector
//! or a facts file.

use ac_domain::tune::rules::{forecast, worst, Forecast, RULES, SHM_MARGIN_MIB};
use ac_domain::tune::{recommend, Finding, HostFacts, Profile, Status};
use rusty_json::{Map, Value};

use crate::factsjson;

/// Everything doctor says about a host.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The profile judged against.
    pub profile: Profile,
    /// Where the facts came from: this host or a facts file.
    pub source: String,
    /// The facts.
    pub facts: HostFacts,
    /// The findings, in rule order.
    pub findings: Vec<Finding>,
    /// What the recommended settings allow.
    pub forecast: Forecast,
}

/// The profile: as given, else `AC_PROFILE` from the gateway's settings, else build.
pub fn profile(given: Option<Profile>, facts: &HostFacts) -> Result<Profile, String> {
    if let Some(p) = given {
        return Ok(p);
    }
    match facts.gateway.known().and_then(|e| e.get("AC_PROFILE")) {
        None => Ok(Profile::Build),
        Some(p) => Profile::parse(p)
            .ok_or_else(|| format!("AC_PROFILE={p} in gateway.env: want idle, build or heavy")),
    }
}

/// Admission's margin: `AC_SHM_MARGIN_MIB` from the gateway's settings, else its default.
fn margin(facts: &HostFacts) -> u64 {
    facts
        .gateway
        .known()
        .and_then(|e| e.get("AC_SHM_MARGIN_MIB")?.parse().ok())
        .unwrap_or(SHM_MARGIN_MIB)
}

/// Judge `facts` for the profile `given` (or the configured one).
pub fn report(facts: HostFacts, given: Option<Profile>, source: &str) -> Result<Report, String> {
    let profile = profile(given, &facts)?;
    Ok(Report {
        profile,
        source: source.to_owned(),
        findings: recommend(&facts, profile),
        forecast: forecast(&facts, profile, margin(&facts)),
        facts,
    })
}

/// 0 when nothing is worse than info, 1 with warnings, 2 with errors.
pub fn exit_code(findings: &[Finding]) -> u8 {
    match worst(findings) {
        Status::Ok | Status::Info => 0,
        Status::Warn => 1,
        Status::Error => 2,
    }
}

fn count(findings: &[Finding], status: Status) -> usize {
    findings.iter().filter(|f| f.status == status).count()
}

/// A finding's columns: a skipped or invalid fact shows as such, with its cause moved into WHY.
fn columns(f: &Finding) -> [String; 6] {
    let (current, why) = match f.current.split_once(": ") {
        Some((kind @ ("skipped" | "invalid"), cause)) => {
            (kind.to_owned(), format!("{cause}; {}", f.reason))
        }
        _ => (f.current.clone(), f.reason.clone()),
    };
    [
        f.setting.clone(),
        current,
        f.recommended.clone(),
        f.status.name().to_owned(),
        f.confidence.name().to_owned(),
        why,
    ]
}

/// The report as a table, then the forecast and a summary.
pub fn table(r: &Report) -> String {
    let head = [
        "SETTING",
        "CURRENT",
        "RECOMMENDED",
        "STATUS",
        "CONFIDENCE",
        "WHY",
    ]
    .map(String::from);
    let rows: Vec<[String; 6]> = std::iter::once(head)
        .chain(r.findings.iter().map(columns))
        .collect();
    let width = |i: usize| {
        rows.iter()
            .map(|row| row[i].chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..5).map(width).collect();
    let mut out = format!(
        "ac-ctl doctor: profile {}, {} ({} MiB MemTotal, {} cores)\n",
        r.profile.name(),
        r.source,
        r.facts.mem_total_mib,
        r.facts.cores
    );
    for row in &rows {
        for (cell, w) in row.iter().zip(&widths) {
            out.push_str(&format!("{cell:<w$}  "));
        }
        out.push_str(&row[5]);
        out.push('\n');
    }
    let fc = &r.forecast;
    out.push_str(&format!(
        "\nforecast at the recommended settings ({}):\n  agents at {} MiB:   {} (admission: \
         ({} MiB /dev/shm - {} margin) / {})\n  builds at once:     {} ({} cores / {} vCPUs)\n  \
         boot:               {}\n",
        fc.confidence.name(),
        fc.guest_mib,
        fc.max_agents,
        fc.shm_mib,
        fc.margin_mib,
        fc.guest_mib,
        fc.builds_at_once,
        r.facts.cores,
        fc.vcpus,
        fc.boot
    ));
    out.push_str(&format!(
        "\n{} ok, {} info, {} warnings, {} errors\n",
        count(&r.findings, Status::Ok),
        count(&r.findings, Status::Info),
        count(&r.findings, Status::Warn),
        count(&r.findings, Status::Error)
    ));
    out
}

fn object(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.to_owned(), v);
    }
    Value::Object(map)
}

/// The report as one JSON object: `status` (the worst), `errors` and `warnings` counts, the
/// findings, the forecast and the facts (which `--facts-file` reads back).
pub fn json(r: &Report) -> String {
    let finding = |f: &Finding| {
        object([
            ("rule", Value::from(f.rule)),
            ("setting", Value::from(f.setting.as_str())),
            ("current", Value::from(f.current.as_str())),
            ("recommended", Value::from(f.recommended.as_str())),
            ("status", Value::from(f.status.name())),
            ("confidence", Value::from(f.confidence.name())),
            ("reason", Value::from(f.reason.as_str())),
        ])
    };
    let fc = &r.forecast;
    let forecast = object([
        ("guest_mib", Value::from(fc.guest_mib)),
        ("vcpus", Value::from(fc.vcpus)),
        ("shm_mib", Value::from(fc.shm_mib)),
        ("margin_mib", Value::from(fc.margin_mib)),
        ("max_agents", Value::from(fc.max_agents)),
        ("builds_at_once", Value::from(fc.builds_at_once)),
        ("confidence", Value::from(fc.confidence.name())),
        ("boot", Value::from(fc.boot)),
    ]);
    object([
        ("profile", Value::from(r.profile.name())),
        ("source", Value::from(r.source.as_str())),
        ("status", Value::from(worst(&r.findings).name())),
        (
            "errors",
            Value::from(count(&r.findings, Status::Error) as u64),
        ),
        (
            "warnings",
            Value::from(count(&r.findings, Status::Warn) as u64),
        ),
        (
            "findings",
            Value::Array(r.findings.iter().map(finding).collect()),
        ),
        ("forecast", forecast),
        ("facts", factsjson::to_json(&r.facts)),
    ])
    .to_json_string_pretty()
}

/// Every rule, with what it rests on and its manual fix.
pub fn explain() -> String {
    RULES
        .iter()
        .map(|r| {
            format!(
                "{}\n  rule:     {}\n  evidence: {}\n  fix:      {}\n",
                r.id, r.rule, r.evidence, r.fix
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> HostFacts {
        factsjson::from_json(include_str!("../../../deploy/hosts/baileyserv.json"))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn with_env(pairs: &[(&str, &str)]) -> HostFacts {
        let mut f = facts();
        f.gateway = ac_domain::tune::Fact::Known(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        f
    }

    fn exit(f: HostFacts) -> u8 {
        let r = report(f, None, "test").unwrap_or_else(|e| panic!("{e}"));
        exit_code(&r.findings)
    }

    #[test]
    fn exit_codes() {
        // This host as shipped: ok (info only).
        assert_eq!(exit(facts()), 0);
        // A warning: the PSI limit unset on the measured shape.
        assert_eq!(exit(with_env(&[])), 1);
        // An error: no /dev/kvm.
        let mut f = facts();
        f.kvm = ac_domain::tune::Fact::Absent;
        assert_eq!(exit(f), 2);
    }

    #[test]
    fn profile_from_gateway_env() {
        let p = |f: &HostFacts| profile(None, f);
        assert_eq!(p(&with_env(&[])), Ok(Profile::Build));
        assert_eq!(p(&with_env(&[("AC_PROFILE", "heavy")])), Ok(Profile::Heavy));
        assert!(p(&with_env(&[("AC_PROFILE", "huge")])).is_err());
        assert_eq!(
            profile(Some(Profile::Idle), &with_env(&[("AC_PROFILE", "heavy")])),
            Ok(Profile::Idle)
        );
    }

    #[test]
    fn table_and_json() {
        let r =
            report(facts(), Some(Profile::Build), "this host").unwrap_or_else(|e| panic!("{e}"));
        let t = table(&r);
        assert!(t.contains("agents at 2048 MiB:   7"), "{t}");
        assert!(
            t.contains("builds at once:     2 (4 cores / 2 vCPUs)"),
            "{t}"
        );
        assert!(
            t.lines().any(|l| l.starts_with("default_memory")
                && l.contains("2048")
                && l.contains("measured")),
            "{t}"
        );
        let j = json(&r);
        let v = Value::parse(&j).unwrap_or_else(|e| panic!("{e}: {j}"));
        assert_eq!(v.get("errors").and_then(Value::as_u64), Some(0));
        assert_eq!(v.get("profile").and_then(Value::as_str), Some("build"));
        assert_eq!(
            v.pointer("/forecast/max_agents").and_then(Value::as_u64),
            Some(7)
        );
        let psi = v.get("findings").and_then(Value::as_array).and_then(|fs| {
            fs.iter()
                .find(|f| f.get("rule").and_then(Value::as_str) == Some("gateway.cpu_psi_max"))
        });
        assert_eq!(
            psi.and_then(|f| f.get("confidence"))
                .and_then(Value::as_str),
            Some("measured input, gate unvalidated")
        );
        // The JSON reads back as the same facts.
        assert_eq!(factsjson::from_json(&j), Ok(r.facts.clone()));
        // A skipped fact shows as "skipped", its cause in WHY.
        let mut f = facts();
        f.gateway = ac_domain::tune::Fact::Skipped(
            "/etc/agent-cloud/gateway.env is root-only: run with sudo".into(),
        );
        let t = table(&report(f, None, "x").unwrap_or_else(|e| panic!("{e}")));
        let psi = t
            .lines()
            .find(|l| l.starts_with("AC_JOB_CPU_PSI_MAX"))
            .unwrap_or_default();
        assert!(
            psi.contains("skipped") && psi.contains("run with sudo"),
            "{psi}"
        );
    }

    #[test]
    fn explain_lists_every_rule() {
        let e = explain();
        assert!(RULES.iter().all(|r| e.contains(r.id)));
    }
}
