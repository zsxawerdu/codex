use std::collections::BTreeMap;

use codex_features::Feature;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolEnvironments;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::LiveEnvironmentResolver;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::TerminalSession;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::WriteStdinRequest;

use super::EffectiveAdditionalPermissions;
use super::parse_arguments;
use super::resolve_tool_environment;
use super::selected_executor;
use super::unified_exec::preparation::ExecCommandLifetime;
use super::unified_exec::preparation::ParsedExecCommand;

const TOOL_NAME: &str = "python_repl";
const KERNEL: &str = include_str!("../../../assets/python_repl.py");
const YIELD_MS: u64 = 1_000;

#[derive(Default)]
pub(crate) struct PythonReplState(Mutex<Option<Kernel>>);

struct Kernel {
    terminal: TerminalSession,
    marker: String,
    ready: bool,
    ready_prefix_len: usize,
}

impl Kernel {
    fn observe(&mut self, output: &str) {
        // A terminal poll can split the ready marker across calls.
        let combined = format!("{}{output}", &self.marker[..self.ready_prefix_len]);
        self.ready |= combined.contains(&self.marker);
        self.ready_prefix_len = (1..self.marker.len())
            .rev()
            .find(|&len| combined.ends_with(&self.marker[..len]))
            .unwrap_or(0);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    code: Option<String>,
    #[serde(default)]
    reset: bool,
}

pub(crate) struct PythonReplHandler;

impl PythonReplHandler {
    pub(crate) fn only_tool(turn: &TurnContext) -> bool {
        turn.config.features.enabled(Feature::PythonReplOnly)
            && turn.extension_data.get::<PythonReplState>().is_some()
    }

    pub(crate) fn available(turn: &TurnContext, environments: &TurnEnvironmentSnapshot) -> bool {
        cfg!(any(target_os = "macos", target_os = "linux"))
            && turn.config.features.enabled(Feature::PythonRepl)
            && turn.config.features.enabled(Feature::UnifiedExecTty)
            && turn.config.features.enabled(Feature::WriteStdinApproval)
            && environments.environments.len() == 1
            && environments
                .primary()
                .is_some_and(|env| !env.environment().is_remote())
    }

    async fn run(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            call_id,
            payload,
            environments,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(error("expected function arguments"));
        };
        let args: Args = parse_arguments(&arguments)?;
        if args.reset && args.code.is_some() {
            return Err(error("send reset and code in separate calls"));
        }

        let state = session
            .services
            .thread_extension_data
            .get::<PythonReplState>()
            .filter(|_| Self::available(&step_context.turn, &step_context.environments))
            .ok_or_else(|| error("the Python REPL is unavailable in this turn"))?;
        let environment = selected_executor(&environments, &step_context.environments)?;
        let manager = &session.services.unified_exec_manager;
        let mut current = state.0.lock().await;
        if args.reset {
            if let Some(kernel) = current.as_ref() {
                if !manager.terminate_terminal(&kernel.terminal).await {
                    return Err(error("could not stop the Python process"));
                }
                *current = None;
            }
            return Ok(output(
                "Python REPL reset. The next cell starts a new process.".into(),
                true,
            ));
        }

        let context = UnifiedExecContext::new(
            session.clone(),
            step_context.clone(),
            cancellation_token,
            call_id,
        );
        if current.is_none() {
            if args.code.is_none() {
                return Err(error("send a Python cell to start the REPL"));
            }
            let launch = json!({
                "cmd": "python3 -I -u -c <Python REPL bootstrap>",
                "tty": true,
                "login": false,
                "yield_time_ms": YIELD_MS,
            })
            .to_string();
            let mut prepared =
                ParsedExecCommand::parse(&launch, &session, &step_context, environment)?
                    .prepare(ExecCommandLifetime::Interactive)?;
            // Use a fixed argv. A kernel must not retain temporary grants inherited
            // from the turn that happened to start it.
            let nonce = Uuid::new_v4().to_string();
            let marker = format!("\n[Python REPL ready {nonce}]\n");
            prepared.resolved.command = vec![
                "python3".into(),
                "-I".into(),
                "-u".into(),
                "-c".into(),
                KERNEL.into(),
                nonce,
            ];
            prepared.permissions = EffectiveAdditionalPermissions {
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                permissions_preapproved: false,
            };
            let launch_output = manager
                .exec_command(
                    ExecCommandRequest {
                        prepared,
                        process_id: manager.allocate_process_id().await,
                        network: step_context.turn.network.clone(),
                    },
                    &context,
                )
                .await
                .map_err(|err| error(format!("could not start Python 3.9 or newer: {err}")))?;
            let Some(process_id) = launch_output.process_id else {
                return Ok(output(
                    format!(
                        "Python REPL failed to start. It requires python3 (3.9 or newer).\n{}",
                        command_text(&launch_output, &marker),
                    ),
                    false,
                ));
            };
            let terminal = manager.resolve_terminal(process_id).await.map_err(error)?;
            let mut kernel = Kernel {
                terminal,
                marker,
                ready: false,
                ready_prefix_len: 0,
            };
            kernel.observe(&launch_output.raw_output.text);
            let startup_notice = (!kernel.ready).then(|| {
                format!(
                    "Python is starting. Poll with {{}} and then resend your cell.\n{}",
                    command_text(&launch_output, &kernel.marker),
                )
            });
            *current = Some(kernel);
            if let Some(notice) = startup_notice {
                return Ok(output(notice, true));
            }
        }

        let kernel = current.as_mut().expect("kernel was resolved above");
        let input = if let Some(code) = args.code {
            if !kernel.ready {
                return Err(error(
                    "Python is busy. Poll with {} or reset with {\"reset\":true}.",
                ));
            }
            kernel.ready = false;
            kernel.ready_prefix_len = 0;
            format!("{}\n", json!(code))
        } else {
            String::new()
        };
        let response = manager
            .write_stdin(
                &context,
                WriteStdinRequest {
                    terminal: &kernel.terminal,
                    input: &input,
                    yield_time_ms: YIELD_MS,
                    max_output_tokens: None,
                    truncation_policy: step_context.settings.model_info.truncation_policy.into(),
                    interaction_event: None,
                },
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err @ UnifiedExecError::UnknownProcessId { .. }) => {
                *current = None;
                return Err(error(format!(
                    "Python process exited and its state was lost: {err}"
                )));
            }
            Err(err @ UnifiedExecError::StdinApproval(_)) => {
                if !input.is_empty() {
                    kernel.ready = true;
                }
                return Err(error(err));
            }
            Err(err) => return Err(error(err)),
        };
        let text = command_text(&response, &kernel.marker);
        if response.process_id.is_none() {
            *current = None;
            return Ok(output(
                format!("{text}\nPython process exited and its state was lost."),
                response.exit_code == Some(0),
            ));
        }
        kernel.observe(&response.raw_output.text);
        let status = if kernel.ready {
            "Python REPL ready."
        } else {
            "Python is busy. Poll with {}."
        };
        Ok(output(format!("{text}\n{status}"), true))
    }
}

impl ToolExecutor<ToolInvocation> for PythonReplHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.into(),
            description: "Run a Python cell in the thread's persistent local interpreter. Variables, imports, and functions survive across cells. Returns stdout, stderr, and the value of the trailing expression. Top-level await is supported. Send {} to poll a running cell or {\"reset\":true} to stop the interpreter and discard its state.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    ("code".into(), JsonSchema::string(Some("Python source for one cell.".into()))),
                    ("reset".into(), JsonSchema::boolean(Some("Stop the current interpreter.".into()))),
                ]),
                None,
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(Self::run(invocation))
    }
}

impl CoreToolRuntime for PythonReplHandler {
    fn live_environment_resolver(&self) -> Option<LiveEnvironmentResolver> {
        Some(|invocation| {
            Box::pin(async {
                resolve_tool_environment(&invocation.step_context.environments, None)
                    .map(ToolEnvironments::single_executor)
            })
        })
    }
}

fn command_text(response: &ExecCommandToolOutput, marker: &str) -> String {
    // Use the configured model output budget even though the raw stream is used
    // above to detect completion.
    response.model_output().replace(marker, "")
}

fn output(text: String, success: bool) -> Box<dyn ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(text, Some(success)))
}

fn error(err: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("Python REPL: {err}"))
}
