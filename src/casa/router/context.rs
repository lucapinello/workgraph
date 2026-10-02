//! Builds the context block the model router sends — **no shortcuts**.
//!
//! WHY THIS EXISTS. The router shipped sending only the message, and the measurement says that is
//! the difference between 9/40 and 1/40 dropped asks (`reviews/EVAL-2026-09-30-routing-classifier.md`,
//! addenda 14–15). Every block below is assembled from state the house ALREADY holds — there is no
//! training and no new service, and it was the single largest lever found in the whole evaluation.
//!
//! The five blocks, and where each comes from:
//!
//! | block | source |
//! |---|---|
//! | **roles + remit** | `household.toml` `[[agent]]` — id, role, domains, and the FULL `personality` prose, which is where the exclusions live ("cooking technique is Bruno's") |
//! | **memory** | `.casa/memory/MEMORY.md` and every `.md` under it, plus `.casa/preferences.jsonl` |
//! | **plan** | the current ISO week's plan file: dinners by day, plus its "Waiting on" section |
//! | **clock** | the local civil day and time |
//! | **history** | supplied by the caller — the listener passes the recent turns of this chat |
//!
//! Deliberately NOT included: anything that would make the decision depend on the model's training
//! data rather than on the household's own state. The block is a statement of fact about this house.

use std::path::Path;

use super::ContextBlock;

/// Cap a block so a pathological file cannot blow up the prompt. The model server takes 128k
/// tokens, so these are generous — they exist to bound a runaway, not to save space.
const ROLES_MAX: usize = 6000;
const MEMORY_MAX: usize = 6000;
const PLAN_MAX: usize = 4000;
const HISTORY_MAX: usize = 4000;

/// Assemble everything the router should know before deciding.
///
/// `history` is the caller's to supply (the listener has the feed; a diagnostic has none) and may be
/// `None`. Every other block is read from disk and is absent — never empty-stringed — when there is
/// genuinely nothing to read, so the model can tell "no memory recorded yet" from "the memory store
/// could not be read".
pub fn build(root: &Path, history: Option<String>) -> ContextBlock {
    ContextBlock {
        roles: roles_block(root),
        memory: memory_block(root),
        plan: plan_block(root),
        history: history.map(|h| clip(h, HISTORY_MAX)),
        clock: Some(clock_block(root)),
    }
}

fn clip(mut s: String, max: usize) -> String {
    if s.len() > max {
        // char-boundary safe: the blocks are ASCII-dominated but memory notes are not.
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[truncated]");
    }
    s
}

/// `[[agent]]` blocks: id, role, declared domains, and the FULL remit prose.
///
/// The prose is the part that matters and the part that was missing: **both Nora and Bruno declare
/// `meals`**, and only the remit says which of them owns what — which is exactly the wrong-owner
/// class measured in the evaluation.
fn roles_block(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("household.toml")).ok()?;
    let mut out = String::from("WHO WORKS HERE (and what each one owns):\n");
    let mut found = false;
    let mut rest = text.as_str();
    while let Some(i) = rest.find("[[agent]]") {
        let after = &rest[i + "[[agent]]".len()..];
        let end = after.find("\n[[").unwrap_or(after.len());
        let blk = &after[..end];
        rest = &after[end..];
        let field = |name: &str| -> Option<String> {
            let pat = format!("{name} =");
            let j = blk.find(&pat)?;
            let v = blk[j + pat.len()..].trim_start();
            if let Some(stripped) = v.strip_prefix("\"\"\"") {
                let e = stripped.find("\"\"\"").unwrap_or(stripped.len());
                Some(stripped[..e].trim().to_string())
            } else {
                let v = v.trim_start_matches('"');
                let e = v.find('"').unwrap_or(v.len());
                Some(v[..e].to_string())
            }
        };
        let Some(id) = field("id") else { continue };
        let role = field("role").unwrap_or_default();
        let domains = field("domains").unwrap_or_default();
        out.push_str(&format!("- {id}"));
        if !role.is_empty() {
            out.push_str(&format!(" — {role}"));
        }
        if !domains.is_empty() {
            let d = domains
                .trim_matches(|c| c == '[' || c == ']')
                .replace('"', "");
            out.push_str(&format!(" (owns: {})", d.trim()));
        }
        if let Some(p) = field("personality") {
            out.push('\n');
            out.push_str(&format!("  {}", p.replace('\n', " ")));
        }
        out.push('\n');
        found = true;
    }
    found.then(|| clip(out, ROLES_MAX))
}

/// The durable memory store + the preference ledger. Absent when neither exists.
fn memory_block(root: &Path) -> Option<String> {
    let mut out =
        String::from("WHAT WE REMEMBER ABOUT THIS HOUSEHOLD (durable, not the schedule):\n");
    let mut found = false;
    let mem = root.join(".casa").join("memory");
    collect_md(&mem, &mut out, &mut found);
    if let Ok(p) = std::fs::read_to_string(root.join(".casa").join("preferences.jsonl")) {
        for line in p.lines().filter(|l| !l.trim().is_empty()).take(40) {
            out.push_str(&format!("- family correction: {}\n", line.trim()));
            found = true;
        }
    }
    found.then(|| clip(out, MEMORY_MAX))
}

/// Walk `.casa/memory` for `.md` files, one level deep plus its subdirectories.
fn collect_md(dir: &Path, out: &mut String, found: &mut bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_md(&p, out, found);
            continue;
        }
        if p.extension().map(|x| x == "md").unwrap_or(false) {
            if let Ok(txt) = std::fs::read_to_string(&p) {
                let t = txt.trim();
                if !t.is_empty() {
                    out.push_str(&format!("- {}\n", t.replace('\n', " ")));
                    *found = true;
                }
            }
        }
    }
}

/// The current week's plan: dinners by day, plus what is waiting on the family. Absent when this
/// week has no plan (which is itself information — the model is not told a week exists when it does
/// not).
fn plan_block(root: &Path) -> Option<String> {
    let week = iso_week_key();
    let path = root.join("plans").join(format!("{week}-family-plan.md"));
    let body = std::fs::read_to_string(&path).ok()?;
    let mut out = String::from("THIS WEEK'S PLAN:\n");
    let mut rows = 0;
    for line in body.lines() {
        let l = line.trim();
        if l.starts_with('|') && l.matches('|').count() >= 4 {
            let cells: Vec<&str> = l.split('|').map(|c| c.trim()).collect();
            // Day | Slot | Dish | Prep  — skip the header and the rule
            if cells.len() >= 4
                && !cells[1].is_empty()
                && !cells[1].starts_with('-')
                && !cells[1].eq_ignore_ascii_case("day")
            {
                // The live table is `Day | Slot | Dish | Prep`, so the DISH is the 4th cell.
                // Using the 3rd sent the slot type ("Fish", "Vegetarian") instead of the dinner,
                // which is worse than useless for "what's for dinner?" — caught by the test below.
                let dish = cells.get(3).copied().unwrap_or(cells[2]);
                out.push_str(&format!("- {}: {}\n", cells[1], dish));
                rows += 1;
            }
        }
    }
    // the plan's own "waiting on the family" section, if it has one
    if let Some(i) = body.find("Waiting on") {
        let tail = &body[i..];
        let end = tail[10..]
            .find("\n## ")
            .map(|x| x + 10)
            .unwrap_or(tail.len().min(1200));
        let section: String = tail[..end].lines().take(14).collect::<Vec<_>>().join(" ");
        out.push_str(&format!(
            "WAITING ON THE FAMILY: {}\n",
            section.replace("  ", " ")
        ));
    }
    (rows > 0).then(|| clip(out, PLAN_MAX))
}

/// Local civil clock — the router must know what day it is for "tonight", "tomorrow", "this week".
fn clock_block(root: &Path) -> String {
    // the plan's absence is worth stating explicitly rather than leaving the model to infer it
    let planned = plan_block(root).is_some();
    format!(
        "NOW: {} — this week is {}",
        chrono::Local::now().format("%A %d %B %Y, %H:%M"),
        if planned {
            "planned"
        } else {
            "NOT planned yet"
        }
    )
}

/// `2026-W41` for the local civil date.
fn iso_week_key() -> String {
    use chrono::Datelike;
    let now = chrono::Local::now();
    format!("{}-W{:02}", now.iso_week().year(), now.iso_week().week())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path) {
        std::fs::create_dir_all(dir.join(".wg")).unwrap();
        std::fs::create_dir_all(dir.join(".casa").join("memory").join("people").join("luca"))
            .unwrap();
        std::fs::create_dir_all(dir.join("plans")).unwrap();
        std::fs::write(
            dir.join("household.toml"),
            "[[agent]]\nid = \"nora\"\nrole = \"meals & nutrition\"\ndomains = [\"meals\", \"nutrition\"]\n\
             personality = \"\"\"Nora plans the week's meals. Cooking technique is Bruno's.\"\"\"\n\n\
             [[agent]]\nid = \"bruno\"\nrole = \"the kitchen\"\ndomains = [\"cooking\"]\n\
             personality = \"\"\"Bruno turns the plan into dinners.\"\"\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(".casa").join("memory").join("MEMORY.md"),
            "# Family memory\n- Luca shops on Saturday MORNING\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(".casa")
                .join("memory")
                .join("people")
                .join("luca")
                .join("x.md"),
            "Coach Mira adjusts training on feedback about aches.",
        )
        .unwrap();
        std::fs::write(
            dir.join(".casa").join("preferences.jsonl"),
            "{\"text\":\"I have to pick up the kids\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("plans")
                .join(format!("{}-family-plan.md", iso_week_key())),
            "**Week of Monday October 5 \u{2192} Sunday October 11**\n\n## 1. Dinners\n\n\
             | Day | Slot | Dish | Prep |\n|---|---|---|---|\n\
             | Monday October 5 | Fish | Cod baked from frozen | ~30 min |\n\
             | Sunday October 11 | Fish | Miso-glazed salmon | ~30 min |\n\n\
             ## 5. Waiting on Luca\n- Sunday's roast: whole bird or just the good bits?\n",
        )
        .unwrap();
    }

    /// NO SHORTCUTS: every block the harness used must be present from a real house layout.
    #[test]
    fn every_block_is_assembled_from_the_house() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path());
        let c = build(
            d.path(),
            Some("Luca: which one?\nassistant: which night?".to_string()),
        );

        let roles = c
            .roles
            .clone()
            .expect("roles must be assembled — this was the missing input");
        assert!(roles.contains("nora"), "the roster: {roles}");
        assert!(roles.contains("meals"), "declared domains: {roles}");
        assert!(
            roles.contains("Cooking technique is Bruno's"),
            "the FULL remit prose, which is where the exclusions live: {roles}"
        );

        let mem = c.memory.clone().expect("memory must be assembled");
        assert!(mem.contains("Saturday MORNING"), "the memory store: {mem}");
        assert!(
            mem.contains("adjusts training"),
            "nested memory files are walked: {mem}"
        );
        assert!(
            mem.contains("pick up the kids"),
            "the preference ledger: {mem}"
        );

        let clock = c.clock.clone().expect("clock must be assembled");
        assert!(
            clock.contains("this week is planned"),
            "the clock states the plan's presence: {clock}"
        );

        let h = c
            .history
            .clone()
            .expect("the caller's history must be carried through");
        assert!(h.contains("which night?"));

        // and the presence string makes it all observable
        assert_eq!(c.present(), "roles=1 memory=1 plan=1 history=1 clock=1");
    }

    /// A house with nothing to read must produce ABSENT blocks, not empty ones — otherwise the
    /// model cannot tell "no memory yet" from "the store could not be read".
    #[test]
    fn an_empty_house_yields_absent_blocks_not_empty_ones() {
        let d = tempfile::tempdir().unwrap();
        let c = build(d.path(), None);
        assert!(c.roles.is_none());
        assert!(c.memory.is_none());
        assert!(c.history.is_none());
        assert!(c.clock.is_some(), "the clock is always known");
        assert!(c.clock.unwrap().contains("NOT planned yet"));
    }

    /// The PLAN must actually reach the model — it was assembled but never carried, which is a
    /// shortcut of exactly the kind this file exists to prevent.
    #[test]
    fn the_plan_reaches_the_block() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path());
        let c = build(d.path(), None);
        let plan = c
            .plan
            .clone()
            .expect("the plan must be carried, not merely consulted");
        assert!(
            plan.contains("Cod baked from frozen"),
            "dinners by day: {plan}"
        );
        assert!(
            plan.contains("WAITING ON THE FAMILY"),
            "and what is owed back: {plan}"
        );
    }

    #[test]
    fn the_week_key_is_an_iso_week() {
        let k = iso_week_key();
        assert!(k.contains("-W"), "{k}");
        let n: u32 = k.split("-W").nth(1).unwrap().parse().unwrap();
        assert!((1..=53).contains(&n), "{k}");
    }
}

/// The recent turns of a chat, for the SITUATION block — read from the group feed the listener
/// already maintains. This is what lets "yes" or "the salmon" be judged as an answer to a question
/// the house asked, which no per-message view can see.
pub fn recent_history(feed_path: &Path, limit: usize) -> Option<String> {
    let text = std::fs::read_to_string(feed_path).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out =
        String::from("THE CHAT SO FAR (most recent last — 'assistant' lines are OURS):\n");
    let mut n = 0;
    for line in lines
        .iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let who = v["sender"].as_str().unwrap_or("");
        let body = v["text"].as_str().unwrap_or("");
        if who.is_empty() || body.is_empty() {
            continue;
        }
        let ours = v["agentId"].is_string();
        out.push_str(&format!(
            "  {}: {}\n",
            if ours {
                format!("{who} (assistant)")
            } else {
                who.to_string()
            },
            body.replace('\n', " ")
        ));
        n += 1;
    }
    (n > 0).then(|| clip(out, HISTORY_MAX))
}
