use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use worksgood::function::{
    self, FunctionInput, InputType, PlanningConfig, TaskTemplate, TraceFunction,
};
use worksgood::graph::{Node, PRIORITY_DEFAULT, Status, Task};
use worksgood::parser::{load_graph, modify_graph};

use super::graph_path;

/// Resolve a `--from` source to a TraceFunction.
///
/// Resolution order (per §5.4 of cross-repo design doc):
/// 1. If source contains `:` → parse as `peer:function-id`, resolve peer, load from peer's functions dir
/// 2. If source ends in `.yaml` or `.yml` → treat as a file path, load directly
/// 3. Otherwise → existing behavior (search local `.wg/functions/`)
fn resolve_function_source(
    source: &str,
    function_id: &str,
    workgraph_dir: &Path,
) -> Result<TraceFunction> {
    if let Some((peer_name, remote_func_id)) = source.split_once(':') {
        // peer:function-id syntax
        let resolved = worksgood::federation::resolve_peer(peer_name, workgraph_dir)?;
        let peer_func_dir = function::functions_dir(&resolved.workgraph_dir);
        function::find_function_by_prefix(&peer_func_dir, remote_func_id)
            .map_err(|e| anyhow::anyhow!("From peer '{}': {}", peer_name, e))
    } else if source.ends_with(".yaml") || source.ends_with(".yml") {
        // Direct file path
        let path = resolve_file_path(source)?;
        function::load_function(&path)
            .map_err(|e| anyhow::anyhow!("Failed to load function from '{}': {}", source, e))
    } else {
        // Treat as a peer name, with function_id as the function to look up
        let resolved = worksgood::federation::resolve_peer(source, workgraph_dir)?;
        let peer_func_dir = function::functions_dir(&resolved.workgraph_dir);
        function::find_function_by_prefix(&peer_func_dir, function_id)
            .map_err(|e| anyhow::anyhow!("From peer '{}': {}", source, e))
    }
}

/// Expand `~/` and resolve to an absolute path.
fn resolve_file_path(path_str: &str) -> Result<PathBuf> {
    let expanded = if let Some(suffix) = path_str.strip_prefix("~/") {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
        home.join(suffix)
    } else {
        PathBuf::from(path_str)
    };

    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    if !abs.exists() {
        anyhow::bail!("File not found: {}", abs.display());
    }

    Ok(abs)
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    function_id: &str,
    from: Option<&str>,
    inputs: &[String],
    input_file: Option<&str>,
    prefix: Option<&str>,
    dry_run: bool,
    after: &[String],
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    // 1. Load trace function: from --from source or local functions dir
    let func = if let Some(source) = from {
        resolve_function_source(source, function_id, dir)?
    } else {
        let func_dir = function::functions_dir(dir);
        function::find_function_by_prefix(&func_dir, function_id)
            .map_err(|e| anyhow::anyhow!("{}", e))?
    };

    // 2. Parse inputs from --input key=value flags and/or --input-file
    let mut provided: HashMap<String, serde_yaml::Value> = HashMap::new();

    // Parse from --input-file first (so --input flags can override)
    if let Some(path) = input_file {
        let file_inputs = parse_input_file(path)?;
        for (k, v) in file_inputs {
            provided.insert(k, v);
        }
    }

    // Parse from --input key=value flags
    for input_str in inputs {
        let (key, value) = parse_input_pair(input_str, &func.inputs)?;
        provided.insert(key, value);
    }

    // 3. Validate inputs against function schema
    let resolved =
        function::validate_inputs(&func.inputs, &provided).map_err(|e| anyhow::anyhow!("{}", e))?;

    // 4. For file_content type: read file at provided path and substitute content
    let final_inputs = resolve_file_contents(&func.inputs, resolved)?;

    // Layer 3: Memory injection for adaptive functions (version >= 3)
    let memory_text = if func.version >= 3 {
        if let Some(ref memory_config) = func.memory {
            let summaries =
                worksgood::function_memory::load_run_summaries(dir, &func.id, memory_config);
            worksgood::function_memory::render_run_summaries(&summaries, &memory_config.include)
        } else {
            "No previous runs recorded.".to_string()
        }
    } else {
        String::new()
    };

    // 5. Generate task ID prefix
    let prefix = prefix
        .map(String::from)
        .or_else(|| {
            final_inputs
                .get("feature_name")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| func.id.clone());

    // 6. Load graph (needed for creating tasks)
    let graph_file = graph_path(dir);
    let mut graph = if graph_file.exists() {
        load_graph(&graph_file).context("Failed to load graph")?
    } else {
        anyhow::bail!("WG not initialized. Run 'wg init' first.");
    };

    // Validate external blocked-by references exist
    for dep in after {
        if graph.get_node(dep).is_none() {
            eprintln!(
                "Warning: external blocker '{}' does not exist in the graph",
                dep
            );
        }
    }

    // Layer 2: Determine task templates (static or planner-generated)
    let task_templates = if let Some(ref planning) = func.planning {
        execute_plan_or_fallback(
            dir,
            &func,
            planning,
            &final_inputs,
            &memory_text,
            &prefix,
            &mut graph,
            &graph_file,
            after.first().map(|s| s.as_str()),
            model,
            dry_run,
        )?
    } else {
        func.tasks.clone()
    };

    // 7. Render every template ONCE, then build the ID map and create tasks.
    //
    // Everything below reads `rendered`, never the raw template. That is not tidiness:
    // an earlier shape rendered inside the create loop while the `assign:` resolution
    // read `template.assign` and re-ran `substitute` on it by hand. Two substitution
    // sites, and the one in `substitute_task_template` was reachable by nothing —
    // you could replace it with `None` and every test stayed green. One render, one
    // reader, so a mutation anywhere on this path is visible downstream.
    let rendered_templates: Vec<TaskTemplate> = task_templates
        .iter()
        .map(|template| {
            let mut rendered = function::substitute_task_template(template, &final_inputs);
            if !memory_text.is_empty() {
                rendered.description = rendered
                    .description
                    .replace("{{memory.run_summaries}}", &memory_text);
            }
            rendered
        })
        .collect();

    let mut id_map: HashMap<String, String> = HashMap::new(); // template_id -> real task_id
    let mut created_ids: Vec<String> = Vec::new();

    // Pre-compute all task IDs so loops_to can reference forward
    for template in &rendered_templates {
        let task_id = format!("{}-{}", prefix, template.template_id);
        if !dry_run && graph.get_node(&task_id).is_some() {
            anyhow::bail!(
                "Task '{}' already exists. Use a different --prefix.",
                task_id
            );
        }
        id_map.insert(template.template_id.clone(), task_id);
    }

    // 7a. Resolve every rendered `assign:` to a real Agent BEFORE a single task is
    //     written.
    //
    // WHY THIS EXISTS. `func apply` is the SECOND HOP of a two-hop dispatch: hop 1 is
    // a cron task assigned to the household's point of contact whose whole body is
    // "run `wg func apply <fn>`", and hop 2 is the task this command mints — the one
    // that actually fans out. Hop 2 used to be born with `assigned: None` and no
    // agent, while its description opened "You are <the coordinator>, bound session
    // <id>…". So the graph said "anyone" and the prompt said "you specifically", and
    // whichever generic worker picked it up wore the name in the text. That is the
    // "one generic agent wearing four name tags" shape the template it renders warns
    // against, one level above where the template can see it.
    //
    // `role_hint` could not close this: it becomes a decorative `role:<Name>` tag
    // that no reader in this codebase consults. An identity has to be an id.
    //
    // Resolution is deliberately the SAME lookup `wg assign` performs
    // (`find_agent_by_prefix` over `.wg/agency/cache/agents`) — one resolver, and the
    // installer that renders the template has already turned the household's roster
    // alias into that content-addressed id. Failure is FATAL and happens here, before
    // `modify_graph`, so a household with a stale/renamed identity gets a loud error
    // instead of a half-applied function whose lead task is orphaned.
    let mut assign_map: HashMap<String, String> = HashMap::new(); // template_id -> full agent id
    {
        let agents_dir = dir.join("agency").join("cache/agents");
        for template in &rendered_templates {
            // `template.assign` here is the RENDERED field produced above, so a
            // function may take its owner as an input (`assign: "{{input.owner}}"`)
            // and there is no second copy of the substitution rule living here.
            let Some(want) = template
                .assign
                .as_deref()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
            else {
                continue;
            };
            let agent = worksgood::agency::find_agent_by_prefix(&agents_dir, &want).map_err(|e| {
                anyhow::anyhow!(
                    "Function '{}': task template '{}' is assigned to '{}', which matches no agent \
                     definition in {} ({}).\n\
                     A template's `assign:` names the identity the task's own body addresses; \
                     creating it unassigned would send the work to a generic worker impersonating \
                     that helper. Re-render the function against this household's roster (the \
                     installer substitutes the placeholder with a live agent id) and re-apply.",
                    func.id,
                    template.template_id,
                    want,
                    agents_dir.display(),
                    e
                )
            })?;
            assign_map.insert(template.template_id.clone(), agent.id);
        }
    }

    for rendered in &rendered_templates {
        let task_id = id_map[&rendered.template_id].clone();

        // Remap after from template_ids to real task_ids
        let mut real_after: Vec<String> = Vec::new();
        for dep in &rendered.after {
            if let Some(real_id) = id_map.get(dep) {
                real_after.push(real_id.clone());
            } else {
                eprintln!(
                    "Warning: after '{}' in template '{}' not found in function",
                    dep, rendered.template_id
                );
            }
        }

        // Add external --after for root tasks (those with no internal after)
        if rendered.after.is_empty() {
            real_after.extend(after.iter().cloned());
        }

        // Build tags: include role_hint as role:<name> tag, plus template tags and skills
        let mut tags = rendered.tags.clone();
        for skill in &rendered.skills {
            if !skill.is_empty() {
                tags.push(format!("skill:{}", skill));
            }
        }
        if let Some(ref role) = rendered.role_hint {
            tags.push(format!("role:{}", role));
        }

        // Apply model: --model flag overrides everything
        let task_model = model.map(String::from);

        if dry_run {
            // Show plan without creating tasks
            print_dry_run_task(
                &task_id,
                rendered,
                &real_after,
                &tags,
                task_model.as_deref(),
                assign_map.get(&rendered.template_id).map(String::as_str),
            );
        } else {
            let task = Task {
                id: task_id.clone(),
                title: rendered.title.clone(),
                description: Some(rendered.description.clone()),
                status: Status::Open,
                priority: PRIORITY_DEFAULT,
                assigned: None,
                estimate: None,
                before: vec![],
                after: real_after.clone(),
                requires: vec![],
                tags,
                skills: rendered.skills.clone(),
                inputs: vec![],
                deliverables: rendered.deliverables.clone(),
                choices: vec![],
                artifacts: vec![],
                exec: None,
                timeout: None,
                not_before: None,
                created_at: Some(Utc::now().to_rfc3339()),
                started_at: None,
                completed_at: None,
                last_interaction_at: None,
                log: vec![],
                retry_count: 0,
                max_retries: None,
                failure_reason: None,
                failure_class: None,
                model: task_model,
                reasoning: None,
                provider: None,
                endpoint: None,
                remote_provider: None,
                profile: None,
                command_argv: vec![],
                working_dir: None,
                executor_preset_name: None,
                verify: rendered.verify.clone(),
                verify_timeout: None,
                // `agent`, not `assigned`: `assigned` is the transient claim slot the
                // dispatcher fills and `cron` clears on every instantiation, while
                // `agent` is the durable identity binding the executor reads to load
                // the helper's role prompt and bound-session memory. It is the field
                // `wg assign` writes and the field the live cadence cron carries.
                agent: assign_map.get(&rendered.template_id).cloned(),
                loop_iteration: 0,
                last_iteration_completed_at: None,
                cycle_failure_restarts: 0,
                ready_after: None,
                paused: false,
                visibility: "internal".to_string(),
                context_scope: None,
                cycle_config: None,
                token_usage: None,
                session_id: None,
                wait_condition: None,
                checkpoint: None,
                triage_count: 0,
                resurrection_count: 0,
                last_resurrected_at: None,
                validation: None,
                validation_commands: vec![],
                validator_agent: None,
                validator_model: None,
                gate_attempts: 0,
                test_required: false,
                rejection_count: 0,
                max_rejections: None,
                exec_mode: None,
                verify_failures: 0,
                rescue_count: 0,
                rescued: false,
                meta_eval_attempts: 0,
                agency_dispatch: None,
                evaluation_lifecycle: None,
                spawn_failures: 0,
                dispatch_count: 0,
                tier: None,
                no_tier_escalation: false,
                tried_models: vec![],
                superseded_by: vec![],
                supersedes: None,
                unplaced: false,
                place_near: vec![],
                place_before: vec![],
                independent: false,
                iteration_round: 0,
                iteration_anchor: None,
                iteration_parent: None,
                iteration_config: None,
                cron_schedule: None,
                cron_enabled: false,
                last_cron_fire: None,
                next_cron_fire: None,
                cron_template: false,
                cron_instance_of: None,
                origin: None,
            };

            graph.add_node(Node::Task(task));

            // Maintain bidirectional consistency: update blocks on blocker tasks
            for dep in &real_after {
                if let Some(blocker) = graph.get_task_mut(dep)
                    && !blocker.before.contains(&task_id)
                {
                    blocker.before.push(task_id.clone());
                }
            }
        }

        created_ids.push(task_id);
    }

    if dry_run {
        if json {
            let output = serde_json::json!({
                "dry_run": true,
                "function_id": func.id,
                "prefix": prefix,
                "task_count": created_ids.len(),
                "task_ids": created_ids,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!(
                "\nDry run: would create {} tasks from function '{}'",
                created_ids.len(),
                func.id
            );
        }
        return Ok(());
    }

    // Save graph atomically
    // The graph was already built up with all tasks; now save atomically via modify_graph
    // We re-load and re-apply since modify_graph handles locking
    let created_ids_for_save = created_ids.clone();
    modify_graph(&graph_file, |existing_graph| {
        // Apply all nodes from our computed graph that don't exist yet
        for id in &created_ids_for_save {
            if existing_graph.get_node(id).is_none() {
                if let Some(node) = graph.get_node(id) {
                    existing_graph.add_node(node.clone());
                }
                // Update bidirectional edges
                if let Some(task) = graph.get_task(id) {
                    for dep in &task.after {
                        if let Some(blocker) = existing_graph.get_task_mut(dep)
                            && !blocker.before.contains(id)
                        {
                            blocker.before.push(id.clone());
                        }
                    }
                }
            }
        }
        !created_ids_for_save.is_empty()
    })
    .context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record provenance
    let config = worksgood::config::Config::load_or_default(dir);
    let input_summary: serde_json::Value = final_inputs
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                serde_json::Value::String(function::render_value(v)),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into();

    let _ = worksgood::provenance::record(
        dir,
        "apply",
        None,
        None,
        serde_json::json!({
            "function_id": func.id,
            "inputs": input_summary,
            "created_task_ids": created_ids,
            "prefix": prefix,
        }),
        config.log.rotation_threshold,
    );

    // Record run for trace memory (append to .runs.jsonl)
    let _ = append_run_record(
        dir,
        &func.id,
        &serde_json::json!({
            "applied_at": Utc::now().to_rfc3339(),
            "inputs": input_summary,
            "prefix": prefix,
            "task_ids": created_ids,
        }),
    );

    // Output
    if json {
        let output = serde_json::json!({
            "function_id": func.id,
            "prefix": prefix,
            "task_count": created_ids.len(),
            "task_ids": created_ids,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Created {} tasks from function '{}':",
            created_ids.len(),
            func.id
        );
        for task_id in &created_ids {
            let task = graph.get_task(task_id).unwrap();
            let blocked_str = if task.after.is_empty() {
                String::new()
            } else {
                format!(" (blocked by {})", task.after.join(", "))
            };
            println!("  {} (Open{})", task_id, blocked_str);
        }
        println!();
        super::print_service_hint(dir);
    }

    Ok(())
}

/// Parse a key=value input pair, converting the value to the appropriate YAML type
/// based on the function's input definitions.
fn parse_input_pair(
    input: &str,
    input_defs: &[FunctionInput],
) -> Result<(String, serde_yaml::Value)> {
    let (key, value_str) = input
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Invalid input format '{}'. Expected key=value", input))?;

    let key = key.trim().to_string();
    let value_str = value_str.trim();

    // Find the input definition to determine the type
    let def = input_defs.iter().find(|d| d.name == key);

    let value = match def.map(|d| &d.input_type) {
        Some(InputType::Number) => {
            // Try to parse as number
            if let Ok(i) = value_str.parse::<i64>() {
                serde_yaml::Value::Number(serde_yaml::Number::from(i))
            } else if let Ok(f) = value_str.parse::<f64>() {
                serde_yaml::Value::Number(serde_yaml::Number::from(f))
            } else {
                anyhow::bail!("Input '{}' should be a number but got '{}'", key, value_str);
            }
        }
        Some(InputType::FileList) => {
            // Comma-separated list of paths
            let items: Vec<serde_yaml::Value> = value_str
                .split(',')
                .map(|s| serde_yaml::Value::String(s.trim().to_string()))
                .collect();
            serde_yaml::Value::Sequence(items)
        }
        Some(InputType::Json) => {
            // Parse as JSON, then convert to YAML value
            let json_val: serde_json::Value = serde_json::from_str(value_str)
                .with_context(|| format!("Input '{}' should be valid JSON", key))?;
            serde_yaml::to_value(&json_val)?
        }
        _ => {
            // String, Text, Url, Enum, FileContent — all are strings from CLI
            serde_yaml::Value::String(value_str.to_string())
        }
    };

    Ok((key, value))
}

/// Parse an input file (YAML or JSON) into a HashMap of values.
fn parse_input_file(path: &str) -> Result<HashMap<String, serde_yaml::Value>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read input file '{}'", path))?;

    // Try YAML first (which is a superset of JSON)
    let mapping: serde_yaml::Value = serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse input file '{}' as YAML", path))?;

    let map = mapping
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("Input file '{}' must contain a YAML mapping", path))?;

    let mut result = HashMap::new();
    for (k, v) in map {
        let key = k
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Input file keys must be strings"))?
            .to_string();
        result.insert(key, v.clone());
    }

    Ok(result)
}

/// For file_content type inputs, read the file at the provided path and
/// replace the value with the file's contents.
fn resolve_file_contents(
    input_defs: &[FunctionInput],
    mut resolved: HashMap<String, serde_yaml::Value>,
) -> Result<HashMap<String, serde_yaml::Value>> {
    for def in input_defs {
        if def.input_type == InputType::FileContent
            && let Some(value) = resolved.get(&def.name)
            && let Some(path) = value.as_str()
        {
            let contents = std::fs::read_to_string(path).with_context(|| {
                format!(
                    "Failed to read file '{}' for file_content input '{}'",
                    path, def.name
                )
            })?;
            resolved.insert(def.name.clone(), serde_yaml::Value::String(contents));
        }
    }
    Ok(resolved)
}

fn print_dry_run_task(
    task_id: &str,
    rendered: &TaskTemplate,
    after: &[String],
    tags: &[String],
    model: Option<&str>,
    agent: Option<&str>,
) {
    println!("  Task: {} (Open)", task_id);
    println!("    Title: {}", rendered.title);
    // The owning identity is the one thing a dry run must not hide: a template that
    // addresses a named helper in its body and lands on nobody is the whole defect.
    if let Some(a) = agent {
        println!("    Agent: {}", a);
    }
    if !after.is_empty() {
        println!("    After: {}", after.join(", "));
    }
    if !rendered.skills.is_empty() {
        println!("    Skills: {}", rendered.skills.join(", "));
    }
    if !tags.is_empty() {
        println!("    Tags: {}", tags.join(", "));
    }
    if let Some(m) = model {
        println!("    Model: {}", m);
    }
    // Show first few lines of description
    let desc_lines: Vec<&str> = rendered.description.lines().take(3).collect();
    if !desc_lines.is_empty() {
        println!("    Description: {}", desc_lines[0]);
        for line in &desc_lines[1..] {
            println!("      {}", line);
        }
        let total_lines = rendered.description.lines().count();
        if total_lines > 3 {
            println!("      ... ({} more lines)", total_lines - 3);
        }
    }
    println!();
}

/// Execute the planning node or fall back to static tasks (Layer 2).
///
/// If a planner task exists in the graph and is Done, parses its output as a
/// YAML task template list, validates against constraints, and returns the
/// generated templates. Otherwise falls back to the function's static tasks.
#[allow(clippy::too_many_arguments)]
fn execute_plan_or_fallback(
    _dir: &Path,
    func: &TraceFunction,
    planning: &PlanningConfig,
    _inputs: &HashMap<String, serde_yaml::Value>,
    _memory_text: &str,
    prefix: &str,
    graph: &mut worksgood::graph::WorkGraph,
    _graph_file: &Path,
    _after: Option<&str>,
    _model: Option<&str>,
    _dry_run: bool,
) -> Result<Vec<TaskTemplate>> {
    let planner_task_id = format!("{}-{}", prefix, planning.planner_template.template_id);

    // Check if planner task exists and is Done
    if let Some(task) = graph.get_task(&planner_task_id)
        && task.status == worksgood::graph::Status::Done
        && let Some(generated) = try_parse_planner_output(task)
    {
        // Validate against constraints if enabled
        if planning.validate_plan
            && let Some(ref constraints) = func.constraints
        {
            match worksgood::plan_validator::validate_plan(&generated, constraints) {
                Ok(()) => return Ok(generated),
                Err(errors) => {
                    eprintln!("Plan validation failed ({} error(s)):", errors.len());
                    for e in &errors {
                        eprintln!("  - {}", e);
                    }
                    if planning.static_fallback {
                        eprintln!("Falling back to static task templates.");
                        return Ok(func.tasks.clone());
                    }
                    anyhow::bail!(
                        "Generated plan failed validation and static_fallback is disabled"
                    );
                }
            }
        }
        return Ok(generated);
    }

    // Planner task not ready — fall back to static templates
    Ok(func.tasks.clone())
}

/// Try to parse planner task output as a list of TaskTemplates.
///
/// Checks artifacts first (for .yaml/.yml files), then log entries for
/// embedded ```yaml blocks.
fn try_parse_planner_output(task: &worksgood::graph::Task) -> Option<Vec<TaskTemplate>> {
    // Check artifacts for YAML files
    for artifact in &task.artifacts {
        if (artifact.ends_with(".yaml") || artifact.ends_with(".yml"))
            && let Ok(content) = std::fs::read_to_string(artifact)
            && let Ok(templates) = serde_yaml::from_str::<Vec<TaskTemplate>>(&content)
            && !templates.is_empty()
        {
            return Some(templates);
        }
    }

    // Check log entries for embedded YAML blocks
    for entry in &task.log {
        if let Some(yaml_str) = extract_yaml_block(&entry.message)
            && let Ok(templates) = serde_yaml::from_str::<Vec<TaskTemplate>>(yaml_str)
            && !templates.is_empty()
        {
            return Some(templates);
        }
    }

    None
}

/// Extract a ```yaml ... ``` fenced code block from text.
fn extract_yaml_block(text: &str) -> Option<&str> {
    let marker = "```yaml\n";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find("```")?;
    let block = rest[..end].trim();
    if block.is_empty() { None } else { Some(block) }
}

/// Append a JSON run record to the function's `.runs.jsonl` file.
fn append_run_record(
    workgraph_dir: &Path,
    function_id: &str,
    record: &serde_json::Value,
) -> Result<()> {
    use std::io::Write;
    let func_dir = workgraph_dir.join("functions");
    std::fs::create_dir_all(&func_dir)?;
    let runs_path = func_dir.join(format!("{}.runs.jsonl", function_id));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&runs_path)?;
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use worksgood::function::*;
    use worksgood::graph::WorkGraph;
    use worksgood::parser::save_graph;

    fn sample_function() -> TraceFunction {
        TraceFunction {
            kind: "trace-function".to_string(),
            version: 1,
            id: "impl-feature".to_string(),
            name: "Implement Feature".to_string(),
            description: "Plan, implement, test a new feature".to_string(),
            extracted_from: vec![],
            extracted_by: None,
            extracted_at: None,
            tags: vec![],
            inputs: vec![
                FunctionInput {
                    name: "feature_name".to_string(),
                    input_type: InputType::String,
                    description: "Short name for the feature".to_string(),
                    required: true,
                    default: None,
                    example: None,
                    min: None,
                    max: None,
                    values: None,
                },
                FunctionInput {
                    name: "test_command".to_string(),
                    input_type: InputType::String,
                    description: "Command to verify".to_string(),
                    required: false,
                    default: Some(serde_yaml::Value::String("cargo test".to_string())),
                    example: None,
                    min: None,
                    max: None,
                    values: None,
                },
            ],
            tasks: vec![
                TaskTemplate {
                    template_id: "plan".to_string(),
                    title: "Plan {{input.feature_name}}".to_string(),
                    description: "Plan the implementation of {{input.feature_name}}".to_string(),
                    skills: vec!["analysis".to_string()],
                    after: vec![],
                    loops_to: vec![],
                    role_hint: Some("analyst".to_string()),
                    assign: None,
                    deliverables: vec![],
                    verify: None,
                    tags: vec![],
                },
                TaskTemplate {
                    template_id: "implement".to_string(),
                    title: "Implement {{input.feature_name}}".to_string(),
                    description: "Implement the feature. Run: {{input.test_command}}".to_string(),
                    skills: vec!["implementation".to_string()],
                    after: vec!["plan".to_string()],
                    loops_to: vec![],
                    role_hint: Some("programmer".to_string()),
                    assign: None,
                    deliverables: vec![],
                    verify: None,
                    tags: vec![],
                },
                TaskTemplate {
                    template_id: "validate".to_string(),
                    title: "Validate {{input.feature_name}}".to_string(),
                    description: "Validate the implementation".to_string(),
                    skills: vec!["review".to_string()],
                    after: vec!["implement".to_string()],
                    loops_to: vec![],
                    role_hint: None,
                    assign: None,
                    deliverables: vec![],
                    verify: None,
                    tags: vec![],
                },
                TaskTemplate {
                    template_id: "refine".to_string(),
                    title: "Refine {{input.feature_name}}".to_string(),
                    description: "Address issues found during validation".to_string(),
                    skills: vec![],
                    after: vec!["validate".to_string()],
                    loops_to: vec![LoopEdgeTemplate {
                        target: "validate".to_string(),
                        max_iterations: 3,
                        guard: None,
                        delay: None,
                    }],
                    role_hint: None,
                    assign: None,
                    deliverables: vec![],
                    verify: None,
                    tags: vec![],
                },
            ],
            outputs: vec![],
            planning: None,
            constraints: None,
            memory: None,
            visibility: FunctionVisibility::Internal,
            redacted_fields: vec![],
        }
    }

    fn setup_workgraph(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let graph = WorkGraph::new();
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();
    }

    fn setup_function(dir: &Path, func: &TraceFunction) {
        let func_dir = function::functions_dir(dir);
        function::save_function(func, &func_dir).unwrap();
    }

    #[test]
    fn instantiate_creates_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("auth-plan").is_some());
        assert!(graph.get_task("auth-implement").is_some());
        assert!(graph.get_task("auth-validate").is_some());
        assert!(graph.get_task("auth-refine").is_some());
    }

    #[test]
    fn instantiate_remaps_after() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let implement = graph.get_task("auth-implement").unwrap();
        assert_eq!(implement.after, vec!["auth-plan"]);

        let validate = graph.get_task("auth-validate").unwrap();
        assert_eq!(validate.after, vec!["auth-implement"]);
    }

    #[test]
    fn instantiate_applies_prefix_override() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            Some("my-prefix"),
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("my-prefix-plan").is_some());
        assert!(graph.get_task("my-prefix-implement").is_some());
    }

    #[test]
    fn instantiate_applies_model() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            Some("sonnet"),
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        for task_id in &[
            "auth-plan",
            "auth-implement",
            "auth-validate",
            "auth-refine",
        ] {
            let task = graph.get_task(task_id).unwrap();
            assert_eq!(task.model, Some("sonnet".to_string()));
        }
    }

    #[test]
    fn instantiate_applies_external_after() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        // Add an external task to block on
        {
            let mut graph = load_graph(dir.join("graph.jsonl")).unwrap();
            graph.add_node(Node::Task(Task {
                id: "prerequisite".to_string(),
                title: "Prerequisite".to_string(),
                ..Task::default()
            }));
            save_graph(&graph, dir.join("graph.jsonl")).unwrap();
        }

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &["prerequisite".to_string()],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        // Root task (plan) should be blocked by the external prerequisite
        let plan = graph.get_task("auth-plan").unwrap();
        assert!(plan.after.contains(&"prerequisite".to_string()));

        // Non-root tasks should NOT have the external after
        let implement = graph.get_task("auth-implement").unwrap();
        assert!(!implement.after.contains(&"prerequisite".to_string()));
        assert!(implement.after.contains(&"auth-plan".to_string()));
    }

    #[test]
    fn instantiate_adds_skill_and_role_tags() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let plan = graph.get_task("auth-plan").unwrap();
        assert!(plan.tags.contains(&"skill:analysis".to_string()));
        assert!(plan.tags.contains(&"role:analyst".to_string()));

        let implement = graph.get_task("auth-implement").unwrap();
        assert!(implement.tags.contains(&"skill:implementation".to_string()));
        assert!(implement.tags.contains(&"role:programmer".to_string()));
    }

    #[test]
    fn instantiate_substitutes_template_values() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &[
                "feature_name=auth".to_string(),
                "test_command=cargo test auth".to_string(),
            ],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let plan = graph.get_task("auth-plan").unwrap();
        assert_eq!(plan.title, "Plan auth");
        assert!(
            plan.description
                .as_ref()
                .unwrap()
                .contains("Plan the implementation of auth")
        );

        let implement = graph.get_task("auth-implement").unwrap();
        assert!(
            implement
                .description
                .as_ref()
                .unwrap()
                .contains("cargo test auth")
        );
    }

    #[test]
    fn instantiate_dry_run_does_not_create_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            true, // dry_run
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("auth-plan").is_none());
        assert!(graph.get_task("auth-implement").is_none());
    }

    #[test]
    fn instantiate_missing_required_input() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        let result = run(
            dir,
            "impl-feature",
            None,
            &[], // missing feature_name
            None,
            None,
            false,
            &[],
            None,
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("feature_name"));
    }

    #[test]
    fn instantiate_function_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        let result = run(
            dir,
            "nonexistent",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn instantiate_duplicate_prefix_fails() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        // First instantiation
        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        // Second instantiation with same prefix should fail
        let result = run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn instantiate_with_input_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        // Create an input file
        let input_file = dir.join("inputs.yaml");
        std::fs::write(
            &input_file,
            "feature_name: auth\ntest_command: cargo test auth\n",
        )
        .unwrap();

        run(
            dir,
            "impl-feature",
            None,
            &[],
            Some(input_file.to_str().unwrap()),
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("auth-plan").is_some());
    }

    #[test]
    fn instantiate_with_file_content_input() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        // Create a function with file_content input
        let mut func = sample_function();
        func.inputs.push(FunctionInput {
            name: "spec".to_string(),
            input_type: InputType::FileContent,
            description: "Spec file".to_string(),
            required: false,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        });
        func.tasks[0].description =
            "Plan {{input.feature_name}} using spec:\n{{input.spec}}".to_string();
        setup_function(dir, &func);

        // Create a spec file
        let spec_file = dir.join("spec.txt");
        std::fs::write(&spec_file, "This is the API spec content").unwrap();

        run(
            dir,
            "impl-feature",
            None,
            &[
                "feature_name=auth".to_string(),
                format!("spec={}", spec_file.to_str().unwrap()),
            ],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let plan = graph.get_task("auth-plan").unwrap();
        assert!(
            plan.description
                .as_ref()
                .unwrap()
                .contains("This is the API spec content")
        );
    }

    #[test]
    fn instantiate_maintains_blocks_symmetry() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let plan = graph.get_task("auth-plan").unwrap();
        assert!(plan.before.contains(&"auth-implement".to_string()));

        let implement = graph.get_task("auth-implement").unwrap();
        assert!(implement.before.contains(&"auth-validate".to_string()));
    }

    #[test]
    fn parse_input_pair_string() {
        let defs = vec![FunctionInput {
            name: "name".to_string(),
            input_type: InputType::String,
            description: "".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }];

        let (k, v) = parse_input_pair("name=hello world", &defs).unwrap();
        assert_eq!(k, "name");
        assert_eq!(v.as_str().unwrap(), "hello world");
    }

    #[test]
    fn parse_input_pair_number() {
        let defs = vec![FunctionInput {
            name: "count".to_string(),
            input_type: InputType::Number,
            description: "".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }];

        let (k, v) = parse_input_pair("count=42", &defs).unwrap();
        assert_eq!(k, "count");
        assert_eq!(v.as_i64().unwrap(), 42);
    }

    #[test]
    fn parse_input_pair_file_list() {
        let defs = vec![FunctionInput {
            name: "files".to_string(),
            input_type: InputType::FileList,
            description: "".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }];

        let (k, v) = parse_input_pair("files=src/main.rs,src/lib.rs", &defs).unwrap();
        assert_eq!(k, "files");
        let seq = v.as_sequence().unwrap();
        assert_eq!(seq.len(), 2);
        assert_eq!(seq[0].as_str().unwrap(), "src/main.rs");
        assert_eq!(seq[1].as_str().unwrap(), "src/lib.rs");
    }

    #[test]
    fn parse_input_pair_unknown_key_defaults_to_string() {
        let defs = vec![];
        let (k, v) = parse_input_pair("unknown=value", &defs).unwrap();
        assert_eq!(k, "unknown");
        assert_eq!(v.as_str().unwrap(), "value");
    }

    #[test]
    fn parse_input_pair_missing_equals() {
        let defs = vec![];
        let result = parse_input_pair("no-equals-sign", &defs);
        assert!(result.is_err());
    }

    #[test]
    fn input_file_yaml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("inputs.yaml");
        std::fs::write(
            &path,
            "feature_name: auth\ncount: 5\nfiles:\n  - a.rs\n  - b.rs\n",
        )
        .unwrap();

        let result = parse_input_file(path.to_str().unwrap()).unwrap();
        assert_eq!(
            result.get("feature_name").unwrap().as_str().unwrap(),
            "auth"
        );
        assert_eq!(result.get("count").unwrap().as_i64().unwrap(), 5);
        assert_eq!(result.get("files").unwrap().as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn input_file_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("inputs.json");
        std::fs::write(&path, r#"{"feature_name": "auth", "count": 5}"#).unwrap();

        let result = parse_input_file(path.to_str().unwrap()).unwrap();
        assert_eq!(
            result.get("feature_name").unwrap().as_str().unwrap(),
            "auth"
        );
        assert_eq!(result.get("count").unwrap().as_i64().unwrap(), 5);
    }

    #[test]
    fn records_provenance() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let ops = worksgood::provenance::read_all_operations(dir).unwrap();
        let apply_ops: Vec<_> = ops.iter().filter(|e| e.op == "apply").collect();
        assert_eq!(apply_ops.len(), 1);

        let detail = &apply_ops[0].detail;
        assert_eq!(detail["function_id"], "impl-feature");
        let created = detail["created_task_ids"].as_array().unwrap();
        assert_eq!(created.len(), 4);
    }

    // ── --from flag tests ──

    #[test]
    fn instantiate_from_file_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        // Save the function to a standalone file (not in functions dir)
        let func = sample_function();
        let func_file = tmp.path().join("external-func.yaml");
        let yaml = serde_yaml::to_string(&func).unwrap();
        std::fs::write(&func_file, yaml).unwrap();

        run(
            dir,
            "impl-feature",
            Some(func_file.to_str().unwrap()),
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("auth-plan").is_some());
        assert!(graph.get_task("auth-implement").is_some());
        assert!(graph.get_task("auth-validate").is_some());
        assert!(graph.get_task("auth-refine").is_some());
    }

    #[test]
    fn instantiate_from_peer() {
        let tmp = TempDir::new().unwrap();

        // Set up "local" WG project
        let local_dir = tmp.path().join("local").join(".wg");
        std::fs::create_dir_all(&local_dir).unwrap();
        let graph = WorkGraph::new();
        save_graph(&graph, local_dir.join("graph.jsonl")).unwrap();

        // Set up "peer" WG project with a function
        let peer_project = tmp.path().join("peer-project");
        let peer_wg_dir = peer_project.join(".wg");
        std::fs::create_dir_all(&peer_wg_dir).unwrap();
        let peer_func_dir = peer_wg_dir.join("functions");
        function::save_function(&sample_function(), &peer_func_dir).unwrap();

        // Add peer to federation config
        let config = worksgood::federation::FederationConfig {
            peers: std::collections::BTreeMap::from([(
                "mypeer".to_string(),
                worksgood::federation::PeerConfig {
                    path: peer_project.to_str().unwrap().to_string(),
                    description: None,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        worksgood::federation::save_federation_config(&local_dir, &config).unwrap();

        // Instantiate from peer
        run(
            &local_dir,
            "impl-feature",
            Some("mypeer:impl-feature"),
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(local_dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("auth-plan").is_some());
        assert!(graph.get_task("auth-implement").is_some());
    }

    #[test]
    fn instantiate_from_peer_name_only() {
        let tmp = TempDir::new().unwrap();

        // Set up local WG project
        let local_dir = tmp.path().join("local").join(".wg");
        std::fs::create_dir_all(&local_dir).unwrap();
        let graph = WorkGraph::new();
        save_graph(&graph, local_dir.join("graph.jsonl")).unwrap();

        // Set up peer WG project with a function
        let peer_project = tmp.path().join("peer-project");
        let peer_wg_dir = peer_project.join(".wg");
        std::fs::create_dir_all(&peer_wg_dir).unwrap();
        let peer_func_dir = peer_wg_dir.join("functions");
        function::save_function(&sample_function(), &peer_func_dir).unwrap();

        // Add peer to federation config
        let config = worksgood::federation::FederationConfig {
            peers: std::collections::BTreeMap::from([(
                "mypeer".to_string(),
                worksgood::federation::PeerConfig {
                    path: peer_project.to_str().unwrap().to_string(),
                    description: None,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        worksgood::federation::save_federation_config(&local_dir, &config).unwrap();

        // Use --from with just the peer name (function_id is the positional arg)
        run(
            &local_dir,
            "impl-feature",
            Some("mypeer"),
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(local_dir.join("graph.jsonl")).unwrap();
        assert!(graph.get_task("auth-plan").is_some());
    }

    #[test]
    fn instantiate_from_nonexistent_file_fails() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        let result = run(
            dir,
            "impl-feature",
            Some("/nonexistent/path/func.yaml"),
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn instantiate_from_nonexistent_peer_fails() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        let result = run(
            dir,
            "impl-feature",
            Some("no-such-peer:impl-feature"),
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        );
        assert!(result.is_err());
    }

    // ── Run tracking tests ──

    #[test]
    fn instantiate_records_run_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let runs_path = dir.join("functions").join("impl-feature.runs.jsonl");
        assert!(runs_path.exists(), "runs.jsonl should be created");

        let content = std::fs::read_to_string(&runs_path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "should have one run record");

        let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(record["prefix"], "auth");
        assert!(record["applied_at"].is_string());
        assert!(record["task_ids"].is_array());
        assert_eq!(record["task_ids"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn instantiate_appends_run_jsonl_on_second_call() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        setup_function(dir, &sample_function());

        // First instantiation
        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        // Second instantiation with different prefix
        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            Some("second"),
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let runs_path = dir.join("functions").join("impl-feature.runs.jsonl");
        let content = std::fs::read_to_string(&runs_path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "should have two run records");
    }

    // ── Memory injection tests ──

    #[test]
    fn instantiate_v3_injects_memory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        // Create a v3 function with memory config and a memory placeholder
        let mut func = sample_function();
        func.version = 3;
        func.memory = Some(TraceMemoryConfig {
            max_runs: 10,
            include: MemoryInclusions {
                outcomes: true,
                scores: true,
                interventions: true,
                duration: true,
                retries: false,
                artifacts: false,
            },
            storage_path: None,
        });
        func.tasks[0].description =
            "Plan {{input.feature_name}}\n\nPast runs:\n{{memory.run_summaries}}".to_string();
        setup_function(dir, &func);

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let plan = graph.get_task("auth-plan").unwrap();
        let desc = plan.description.as_ref().unwrap();

        // With no prior runs, memory should be replaced with "No previous runs recorded."
        assert!(
            desc.contains("No previous runs recorded."),
            "v3 function should inject memory text; got: {}",
            desc
        );
        assert!(
            !desc.contains("{{memory.run_summaries}}"),
            "placeholder should be replaced"
        );
    }

    #[test]
    fn instantiate_v1_does_not_inject_memory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        // Create a v1 function with a memory placeholder (shouldn't be replaced)
        let mut func = sample_function();
        func.tasks[0].description =
            "Plan {{input.feature_name}}\n{{memory.run_summaries}}".to_string();
        setup_function(dir, &func);

        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let plan = graph.get_task("auth-plan").unwrap();
        let desc = plan.description.as_ref().unwrap();

        // v1 function: memory_text is empty, so placeholder stays
        assert!(
            desc.contains("{{memory.run_summaries}}"),
            "v1 function should NOT inject memory; got: {}",
            desc
        );
    }

    // ── Plan execution tests ──

    #[test]
    fn instantiate_v2_falls_back_to_static_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        // Create a v2 function with planning config
        let mut func = sample_function();
        func.version = 2;
        func.planning = Some(PlanningConfig {
            planner_template: TaskTemplate {
                template_id: "planner".to_string(),
                title: "Plan it".to_string(),
                description: "Generate a plan".to_string(),
                skills: vec!["analysis".to_string()],
                after: vec![],
                loops_to: vec![],
                role_hint: None,
                assign: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            output_format: "task-graph-yaml".to_string(),
            static_fallback: true,
            validate_plan: true,
        });
        func.constraints = Some(StructuralConstraints {
            min_tasks: Some(2),
            max_tasks: Some(10),
            required_skills: vec![],
            max_depth: None,
            allow_cycles: false,
            max_total_iterations: None,
            required_phases: vec![],
            forbidden_patterns: vec![],
        });
        setup_function(dir, &func);

        // No planner task exists, so should fall back to static templates
        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        // Static fallback tasks should be created
        assert!(graph.get_task("auth-plan").is_some());
        assert!(graph.get_task("auth-implement").is_some());
        assert!(graph.get_task("auth-validate").is_some());
        assert!(graph.get_task("auth-refine").is_some());
    }

    #[test]
    fn instantiate_v2_uses_planner_output() {
        use worksgood::graph::{LogEntry, Node, Status, Task};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        // Create a v2 function with planning
        let mut func = sample_function();
        func.version = 2;
        func.planning = Some(PlanningConfig {
            planner_template: TaskTemplate {
                template_id: "planner".to_string(),
                title: "Plan it".to_string(),
                description: "Generate a plan".to_string(),
                skills: vec![],
                after: vec![],
                loops_to: vec![],
                role_hint: None,
                assign: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            output_format: "task-graph-yaml".to_string(),
            static_fallback: true,
            validate_plan: false, // skip validation for this test
        });
        // Clear static tasks to verify we get planner output
        func.tasks = vec![];
        setup_function(dir, &func);

        // Create the planner task in Done state with YAML output in log
        let planner_yaml = r#"```yaml
- template_id: design
  title: "Design auth"
  description: "Design the auth system"
  skills: [analysis]
- template_id: build
  title: "Build auth"
  description: "Build the auth system"
  skills: [implementation]
  after: [design]
```"#;

        let mut graph = load_graph(dir.join("graph.jsonl")).unwrap();
        graph.add_node(Node::Task(Task {
            id: "auth-planner".to_string(),
            title: "Plan it".to_string(),
            status: Status::Done,
            log: vec![LogEntry {
                timestamp: "2026-02-21T12:00:00Z".to_string(),
                actor: Some("agent".to_string()),
                user: Some(worksgood::current_user()),
                message: planner_yaml.to_string(),
            }],
            ..Task::default()
        }));
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();

        // Run instantiation
        run(
            dir,
            "impl-feature",
            None,
            &["feature_name=auth".to_string()],
            None,
            None,
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        // Should use planner output, not static tasks
        assert!(
            graph.get_task("auth-design").is_some(),
            "planner-generated 'design' task should exist"
        );
        assert!(
            graph.get_task("auth-build").is_some(),
            "planner-generated 'build' task should exist"
        );
    }

    // ── extract_yaml_block tests ──

    #[test]
    fn extract_yaml_block_basic() {
        let text = "Here is the plan:\n```yaml\n- id: a\n  title: A\n```\nDone.";
        let block = extract_yaml_block(text).unwrap();
        assert!(block.contains("- id: a"));
        assert!(block.contains("title: A"));
    }

    #[test]
    fn extract_yaml_block_no_yaml() {
        let text = "No yaml here.";
        assert!(extract_yaml_block(text).is_none());
    }

    #[test]
    fn extract_yaml_block_empty() {
        let text = "```yaml\n```";
        assert!(extract_yaml_block(text).is_none());
    }

    // ── `assign:` — the second hop of a two-hop dispatch must land on a helper ──
    //
    // THE DEFECT THESE LOCK. A household cadence cron (hop 1) is assigned to the
    // point of contact and its whole body is "run `wg func apply <fn>`". The task
    // that call mints (hop 2) is the one that actually does the work — and it was
    // born with no owner at all while its description opened "You are <the
    // coordinator>, bound session <id>…". The graph said "anybody", the prompt said
    // "you specifically", and any generic worker that picked it up wore the name.
    // `role_hint` could not fix it: it becomes a `role:<Name>` tag that no reader in
    // this codebase consults, which the first test below states as an assertion so
    // nobody re-reaches for it.

    /// Write a minimal but REAL agent definition into the agency store, so these
    /// tests resolve through `find_agent_by_prefix` — the same lookup `wg assign`
    /// uses — rather than through a stub that would pass whatever we wrote.
    fn seed_agent(dir: &Path, id: &str, name: &str) {
        let agents_dir = dir.join("agency").join("cache/agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join(format!("{}.yaml", id)),
            format!(
                "id: {id}\nrole_id: role-fixture\ntradeoff_id: tradeoff-fixture\n\
                 name: {name}\nperformance:\n  task_count: 0\n  avg_score: null\n"
            ),
        )
        .unwrap();
    }

    fn one_task_function(id: &str, assign: Option<&str>, role_hint: Option<&str>) -> TraceFunction {
        let mut func = sample_function();
        func.id = id.to_string();
        func.inputs = vec![];
        func.tasks = vec![TaskTemplate {
            template_id: "plan".to_string(),
            title: "Draft next week's plan".to_string(),
            description: "You are the coordinator. Draft the week.".to_string(),
            skills: vec![],
            after: vec![],
            loops_to: vec![],
            role_hint: role_hint.map(String::from),
            assign: assign.map(String::from),
            deliverables: vec![],
            verify: None,
            tags: vec![],
        }];
        func
    }

    const FIXTURE_AGENT: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn apply_binds_template_assign_to_the_tasks_agent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        seed_agent(dir, FIXTURE_AGENT, "Household Coordinator");
        // A PREFIX, not the full hash — the same abbreviation `wg assign` accepts,
        // proving we resolve through the store instead of copying the string across.
        setup_function(
            dir,
            &one_task_function("cadence", Some(&FIXTURE_AGENT[..12]), None),
        );

        run(
            dir,
            "cadence",
            None,
            &[],
            None,
            Some("hop2"),
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let task = graph.get_task("hop2-plan").expect("hop 2 task created");
        assert_eq!(
            task.agent.as_deref(),
            Some(FIXTURE_AGENT),
            "the minted task must be BOUND to the identity its own body addresses; \
             an unowned hop-2 is dispatched to a generic worker impersonating it"
        );
    }

    #[test]
    fn apply_refuses_an_assign_that_names_no_agent_and_creates_nothing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        seed_agent(dir, FIXTURE_AGENT, "Household Coordinator");
        setup_function(
            dir,
            &one_task_function("cadence", Some("nobody-by-that-name"), None),
        );

        let err = run(
            dir,
            "cadence",
            None,
            &[],
            None,
            Some("hop2"),
            false,
            &[],
            None,
            false,
        )
        .expect_err("an assignment to a non-existent identity must be fatal");
        let msg = err.to_string();
        assert!(
            msg.contains("nobody-by-that-name") && msg.contains("plan"),
            "the refusal must name the unresolvable id and the template: {msg}"
        );

        // Fail CLOSED: refusing after writing the task would leave the household with
        // exactly the orphan this change exists to prevent.
        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        assert!(
            graph.get_task("hop2-plan").is_none(),
            "a refused apply must not leave a half-created, unowned task behind"
        );
    }

    #[test]
    fn role_hint_alone_does_not_bind_an_owner() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        seed_agent(dir, FIXTURE_AGENT, "Household Coordinator");
        setup_function(
            dir,
            &one_task_function("cadence", None, Some("Household Coordinator")),
        );

        run(
            dir,
            "cadence",
            None,
            &[],
            None,
            Some("hop2"),
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let task = graph.get_task("hop2-plan").unwrap();
        // Documented, not desired: the role becomes a decorative tag and the task
        // stays unowned. This is the state the household's plan task shipped in, and
        // it is why `assign:` had to exist. If a future change makes `role_hint`
        // resolve to an identity, this assertion should be the one that fails.
        assert!(
            task.tags
                .contains(&"role:Household Coordinator".to_string()),
            "role_hint still renders its tag"
        );
        assert_eq!(
            task.agent, None,
            "role_hint is a label, not a binding — only `assign:` owns a task"
        );
    }

    #[test]
    fn apply_resolves_an_assign_carried_in_an_input() {
        // TEETH FOR THE SUBSTITUTION ITSELF. The previous round added `assign:` to
        // `substitute_task_template` and reported a mutation proof for it; the
        // substitution was unreachable (the resolver re-substituted by hand off the
        // RAW template) and no test could see it either way. This one can only pass
        // if the value the resolver reads went through `substitute`: the template
        // never contains an agent id, only `{{input.owner}}`, and an unsubstituted
        // placeholder resolves to no agent and aborts the apply.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);
        seed_agent(dir, FIXTURE_AGENT, "Household Coordinator");

        let mut func = one_task_function("cadence", Some("{{input.owner}}"), None);
        func.inputs = vec![FunctionInput {
            name: "owner".to_string(),
            input_type: InputType::String,
            description: "Agent id of the helper this plan belongs to".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }];
        setup_function(dir, &func);

        run(
            dir,
            "cadence",
            None,
            &[format!("owner={}", &FIXTURE_AGENT[..12])],
            None,
            Some("hop2"),
            false,
            &[],
            None,
            false,
        )
        .unwrap();

        let graph = load_graph(dir.join("graph.jsonl")).unwrap();
        let task = graph.get_task("hop2-plan").expect("hop 2 task created");
        assert_eq!(
            task.agent.as_deref(),
            Some(FIXTURE_AGENT),
            "`assign: {{{{input.owner}}}}` must render before it is resolved"
        );
    }

    #[test]
    fn assign_survives_the_yaml_round_trip() {
        // The household template carries `assign:` as YAML text rendered by the
        // installer. A field the parser silently drops would put the defect back
        // with the fix still in the tree, and nothing else in this file would notice.
        let yaml = "kind: trace-function\nversion: 2\nid: cadence\nname: Cadence\n\
                    description: d\ninputs: []\ntasks:\n- template_id: plan\n  \
                    title: t\n  role_hint: Family Assistant\n  \
                    assign: \"1111111111111111111111111111111111111111111111111111111111111111\"\n  \
                    description: d\noutputs: []\n";
        let func: TraceFunction = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            func.tasks[0].assign.as_deref(),
            Some(FIXTURE_AGENT),
            "`assign:` must survive deserialization of a rendered function"
        );
    }
}
