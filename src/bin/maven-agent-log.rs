use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use maven_mcp::log_analysis::{
    CommandCategory, CommandEvent, EventFilter, LogSource, discover_default_sources, filter_events,
    read_events, redact_events,
};
use serde::Serialize;
use walkdir::WalkDir;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Extract and analyze real agent terminal commands without an MCP server or LLM"
)]
struct Cli {
    /// Log files or directories to inspect.
    #[arg(value_name = "PATH")]
    inputs: Vec<PathBuf>,
    /// Discover common VS Code, Codex, Kilo Code, and IntelliJ storage roots.
    #[arg(long)]
    discover: bool,
    /// Explicit source format; auto infers it from each filename.
    #[arg(long, value_enum, default_value_t = SourceArg::Auto)]
    source: SourceArg,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
    format: OutputFormat,
    /// Write the report to a file instead of stdout.
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    ide: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long, value_enum)]
    category: Option<CategoryArg>,
    /// Group counts by a normalized event field.
    #[arg(long, value_enum)]
    group_by: Option<GroupBy>,
    /// Project root replaced with <WORKSPACE> in reports.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Additional literal value to replace with <REDACTED>; repeatable.
    #[arg(long = "redact-pattern")]
    redact_patterns: Vec<String>,
    /// Emit paths and credentials without the default privacy redaction.
    #[arg(long)]
    unsafe_no_redact: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Auto,
    VsCode,
    VsCodeInsiders,
    Codex,
    Kilo,
    IntelliJ,
}

impl From<SourceArg> for LogSource {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::Auto => Self::Auto,
            SourceArg::VsCode => Self::VsCode,
            SourceArg::VsCodeInsiders => Self::VsCodeInsiders,
            SourceArg::Codex => Self::Codex,
            SourceArg::Kilo => Self::Kilo,
            SourceArg::IntelliJ => Self::IntelliJ,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Terminal,
    Json,
    Jsonl,
    Csv,
    Markdown,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CategoryArg {
    Maven,
    JarInspection,
    ClassInspection,
    PomInspection,
    ResourceInspection,
    Shell,
}

impl From<CategoryArg> for CommandCategory {
    fn from(value: CategoryArg) -> Self {
        match value {
            CategoryArg::Maven => Self::Maven,
            CategoryArg::JarInspection => Self::JarInspection,
            CategoryArg::ClassInspection => Self::ClassInspection,
            CategoryArg::PomInspection => Self::PomInspection,
            CategoryArg::ResourceInspection => Self::ResourceInspection,
            CategoryArg::Shell => Self::Shell,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GroupBy {
    Ide,
    Agent,
    Project,
    Session,
    Category,
}

#[derive(Debug, Serialize)]
struct GroupCount {
    group: String,
    count: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = collect_paths(&cli)?;
    let mut events = Vec::new();
    let explicit_source = LogSource::from(cli.source);
    for path in paths {
        match read_events(&path, explicit_source) {
            Ok(mut parsed) => events.append(&mut parsed),
            Err(error) if matches!(cli.source, SourceArg::Auto) => {
                eprintln!("skipping {}: {error}", path.display());
            }
            Err(error) => return Err(error).with_context(|| path.display().to_string()),
        }
    }
    events.sort_by(|left, right| {
        (&left.timestamp, &left.ide, &left.session, &left.command).cmp(&(
            &right.timestamp,
            &right.ide,
            &right.session,
            &right.command,
        ))
    });
    events.dedup_by(|left, right| {
        left.ide == right.ide
            && left.session == right.session
            && left.tool == right.tool
            && left.cwd == right.cwd
            && left.command == right.command
    });
    let filter = EventFilter {
        since: cli.since,
        until: cli.until,
        ide: cli.ide,
        agent: cli.agent,
        project: cli.project,
        session: cli.session,
        category: cli.category.map(Into::into),
    };
    let mut events = filter_events(events, &filter);
    if !cli.unsafe_no_redact {
        redact_events(&mut events, cli.workspace.as_deref(), &cli.redact_patterns);
    }
    let report = if let Some(group_by) = cli.group_by {
        render_groups(&group_events(&events, group_by), cli.format)?
    } else {
        render_events(&events, cli.format)?
    };
    if let Some(output) = cli.output {
        std::fs::write(&output, report)
            .with_context(|| format!("cannot write {}", output.display()))?;
    } else {
        print!("{report}");
    }
    Ok(())
}

fn collect_paths(cli: &Cli) -> Result<Vec<PathBuf>> {
    let mut roots = cli.inputs.clone();
    if cli.discover {
        roots.extend(discover_default_sources());
    }
    if roots.is_empty() {
        bail!("provide at least one PATH or use --discover")
    }
    let mut paths = Vec::new();
    for root in roots {
        if root.is_file() {
            paths.push(root);
        } else if root.is_dir() {
            for entry in WalkDir::new(root).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_file() && supported_file(entry.path()) {
                    paths.push(entry.into_path());
                }
            }
        } else {
            bail!("input path does not exist: {}", root.display())
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn supported_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("json" | "jsonl" | "db" | "sqlite" | "sqlite3" | "log" | "txt")
    )
}

fn group_events(events: &[CommandEvent], group_by: GroupBy) -> Vec<GroupCount> {
    let mut counts = BTreeMap::new();
    for event in events {
        let group = match group_by {
            GroupBy::Ide => event.ide.clone(),
            GroupBy::Agent => event
                .agent
                .clone()
                .unwrap_or_else(|| "<unknown>".to_owned()),
            GroupBy::Project => event
                .project
                .clone()
                .unwrap_or_else(|| "<unknown>".to_owned()),
            GroupBy::Session => event
                .session
                .clone()
                .unwrap_or_else(|| "<unknown>".to_owned()),
            GroupBy::Category => format!("{:?}", event.category).to_ascii_lowercase(),
        };
        *counts.entry(group).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(group, count)| GroupCount { group, count })
        .collect()
}

fn render_events(events: &[CommandEvent], format: OutputFormat) -> Result<String> {
    Ok(match format {
        OutputFormat::Terminal => events
            .iter()
            .map(|event| {
                format!(
                    "{} {:<17} {:<20} {}\n",
                    event.timestamp.as_deref().unwrap_or("-"),
                    format!("{:?}", event.category).to_ascii_lowercase(),
                    event.ide,
                    event.command
                )
            })
            .collect(),
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(events)?),
        OutputFormat::Jsonl => {
            events
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Vec<_>>>()?
                .join("\n")
                + "\n"
        }
        OutputFormat::Csv => {
            let mut output =
                "timestamp,ide,agent,project,session,tool,cwd,category,command\n".to_owned();
            for event in events {
                output.push_str(
                    &[
                        event.timestamp.as_deref().unwrap_or(""),
                        &event.ide,
                        event.agent.as_deref().unwrap_or(""),
                        event.project.as_deref().unwrap_or(""),
                        event.session.as_deref().unwrap_or(""),
                        &event.tool,
                        event.cwd.as_deref().unwrap_or(""),
                        &format!("{:?}", event.category).to_ascii_lowercase(),
                        &event.command,
                    ]
                    .into_iter()
                    .map(csv_field)
                    .collect::<Vec<_>>()
                    .join(","),
                );
                output.push('\n');
            }
            output
        }
        OutputFormat::Markdown => {
            let mut output =
                "| Time | IDE | Category | Command |\n| --- | --- | --- | --- |\n".to_owned();
            for event in events {
                output.push_str(&format!(
                    "| {} | {} | {:?} | `{}` |\n",
                    markdown(event.timestamp.as_deref().unwrap_or("-")),
                    markdown(&event.ide),
                    event.category,
                    markdown(&event.command)
                ));
            }
            output
        }
    })
}

fn render_groups(groups: &[GroupCount], format: OutputFormat) -> Result<String> {
    Ok(match format {
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(groups)?),
        OutputFormat::Jsonl => {
            groups
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Vec<_>>>()?
                .join("\n")
                + "\n"
        }
        OutputFormat::Csv => {
            let mut output = "group,count\n".to_owned();
            for group in groups {
                output.push_str(&format!("{},{}\n", csv_field(&group.group), group.count));
            }
            output
        }
        OutputFormat::Markdown => {
            let mut output = "| Group | Count |\n| --- | ---: |\n".to_owned();
            for group in groups {
                output.push_str(&format!(
                    "| {} | {} |\n",
                    markdown(&group.group),
                    group.count
                ));
            }
            output
        }
        OutputFormat::Terminal => groups
            .iter()
            .map(|group| format!("{:<30} {}\n", group.group, group.count))
            .collect(),
    })
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn markdown(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace('\n', " ")
        .replace('`', "\\`")
}
