//! Explicit live acceptance check. Uses the owner's configured model; never runs in normal tests.
//! cargo run -p pdfcraft-automation --features html-review --example copilot_check -- FILE --allow-workspace-operations --apply
use pdfcraft_automation::{Automation, Content};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

fn call(a: &mut Automation, name: &str, args: Value) -> Result<Value, String> {
    a.call(name, &args)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find_map(|c| match c {
            Content::Json(v) => Some(v),
            _ => None,
        })
        .ok_or_else(|| "missing structured result".into())
}
fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().ok_or_else(|| "provide a synthetic test document".to_string())?;
    let allow = args.iter().any(|a| a == "--allow-workspace-operations");
    let apply = args.iter().any(|a| a == "--apply");
    let mut a = Automation::new();
    a.copilot_backend = pdfcraft_ai::configuration::configured_backend()?;
    let opened = call(&mut a, "doc_open", json!({"path":path}))?;
    let id = opened.get("doc").and_then(Value::as_u64).ok_or_else(|| "missing document id".to_string())?;
    let original = call(&mut a, "comment_list", json!({"doc":id}))?;
    let request = if apply {
        "For this synthetic acceptance check, propose exactly one comment on page 1 with text 'Copilot acceptance note'. Use the native change-packet protocol provided in this turn. Write the packet using your file tools, not just an explanation. Do not change any other document content."
    } else {
        "In one short sentence, explain the main idea of this page. Do not edit any files."
    };
    call(&mut a, "copilot_chat", json!({"doc":id,"page":1,"message":request}))?;
    let started = Instant::now();
    let mut approvals = 0u32;
    let status = loop {
        if started.elapsed() > Duration::from_secs(210) {
            let _ = call(&mut a, "copilot_stop", json!({"doc":id}));
            return Err("live copilot deadline exceeded".into());
        }
        let status = call(&mut a, "copilot_status", json!({"doc":id}))?;
        if let Some(permission) = status.get("permission").filter(|p| p.is_object()) {
            approvals = approvals.saturating_add(1);
            if approvals > 12 {
                return Err("too many operation approval requests".into());
            }
            call(&mut a, "copilot_respond", json!({"doc":id,"request_id":permission.get("request_id"),"allow_once":allow}))?;
        }
        if status.get("finished").and_then(Value::as_bool) == Some(true) {
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if let Some(error) = status.get("error").and_then(Value::as_str) {
        return Err(error.into());
    }
    if status.get("answer").and_then(Value::as_str).unwrap_or_default().is_empty() {
        return Err("agent returned no answer".into());
    }
    let mut saved = false;
    if apply {
        if !status.get("proposal").is_some_and(Value::is_object) {
            return Err("agent did not create the requested change packet".into());
        }
        call(&mut a, "copilot_apply_proposal", json!({"doc":id}))?;
        let changed = call(&mut a, "comment_list", json!({"doc":id}))?;
        if changed == original {
            return Err("accepted proposal did not add a comment".into());
        }
        call(&mut a, "edit_undo", json!({"doc":id}))?;
        if call(&mut a, "comment_list", json!({"doc":id}))? != original {
            return Err("undo did not restore the original comments".into());
        }
        call(&mut a, "edit_redo", json!({"doc":id}))?;
        let root = status.get("workspace").and_then(Value::as_str).ok_or_else(|| "missing workspace".to_string())?;
        let extension = if Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "html" | "htm" | "xhtml"))
        {
            "html"
        } else {
            "pdf"
        };
        let out = Path::new(root).join("outputs").join(format!("native-reviewed.{extension}"));
        call(&mut a, "doc_save", json!({"doc":id,"path":out.to_string_lossy()}))?;
        let reopened = call(&mut a, "doc_open", json!({"path":out.to_string_lossy()}))?;
        let comments = call(&mut a, "comment_list", json!({"doc":reopened.get("doc")}))?;
        if !comments.to_string().contains("Copilot acceptance note") {
            return Err("saved document lost the comment".into());
        }
        saved = true;
    }
    call(&mut a, "copilot_stop", json!({"doc":id}))?;
    println!(
        "{}",
        json!({"live_agent_answer":true,"proposal_applied_saved_reopened":saved,"undo_redo_checked":saved,"operation_approvals":approvals,"seconds":started.elapsed().as_secs(),"answer":status.get("answer")})
    );
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
