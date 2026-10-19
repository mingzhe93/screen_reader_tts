use super::super::{ToolFuture, ToolLimits, ToolResponse, ToolServices, ToolSpec, TimeoutSetting};
use crate::assistant_sandbox::SandboxOutput;
use std::sync::{atomic::AtomicBool, Arc};

pub(in crate::assistant_tools) const SPEC: ToolSpec = ToolSpec {
    name: "run_python",
    description: "Python 3.14 standard library only; fresh interpreter; no network or writable files; print the result. No numpy, pandas or charts.",
    argument: "code",
    max_argument_bytes: 65536,
    timeout: TimeoutSetting { default_secs: 60, min_secs: 1, max_secs: 3600 },
    legacy_timeout: Some(|limits: &ToolLimits| limits.code_timeout_secs),
    execute,
};
fn execute<'a>(services: &'a ToolServices, argument: &'a str, cancel: Arc<AtomicBool>) -> ToolFuture<'a> {
    Box::pin(async move {
        let result = services.sandbox.run(argument.to_string(), services.timeout(SPEC.name)?, cancel).await?;
        result_response(result)
    })
}
fn result_response(result: SandboxOutput) -> anyhow::Result<ToolResponse> {
    let status = if result.cancelled {"cancelled"} else if result.timed_out {"timeout"}
        else if result.out_of_memory || !result.stderr.is_empty() {"error"} else {"ok"};
    // Say which limit stopped the run: the output alone is empty when memory runs out.
    let (summary, note) = if result.cancelled {
        ("Python was stopped.", "")
    } else if result.timed_out {
        ("Python stopped: it ran past its time limit.", "\n[Stopped: the run exceeded its time limit.]")
    } else if result.out_of_memory {
        ("Python stopped: it used more than 256 MB of memory.", "\n[Stopped: the 256 MB memory limit was reached.]")
    } else if !result.stderr.is_empty() {
        ("Python raised an error.", "")
    } else {
        ("Python finished.", "")
    };
    let output = Some(format!("{}{}{}", result.stdout, result.stderr, note));
    Ok(ToolResponse {status,summary:summary.into(),data:serde_json::to_value(result)?,sources:Vec::new(),output})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sandbox_flags_and_captured_output_are_preserved() {
        for (result,expected) in [
            (SandboxOutput {stdout:"391\n".into(),..SandboxOutput::default()},"ok"),
            (SandboxOutput {cancelled:true,..SandboxOutput::default()},"cancelled"),
            (SandboxOutput {timed_out:true,..SandboxOutput::default()},"timeout"),
            (SandboxOutput {out_of_memory:true,..SandboxOutput::default()},"error"),
            (SandboxOutput {stderr:"Traceback".into(),..SandboxOutput::default()},"error"),
        ] {
            let response = result_response(result).unwrap();
            assert_eq!(response.status,expected);
            let output = response.output.unwrap();
            if expected == "ok" { assert_eq!(output, "391\n"); assert_eq!(response.summary, "Python finished."); }
            else if response.data["stderr"] == "Traceback" { assert_eq!(output, "Traceback"); assert_eq!(response.summary, "Python raised an error."); }
            else if response.data["out_of_memory"] == true { assert!(output.contains("256 MB memory limit")); assert!(response.summary.contains("256 MB")); }
            else if response.data["timed_out"] == true { assert!(output.contains("time limit")); assert!(response.summary.contains("time limit")); }
            else { assert_eq!(output, ""); assert_eq!(response.summary, "Python was stopped."); }
        }
    }
}
