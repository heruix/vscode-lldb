use globset;
use regex;
use serde_json;

use std;
use std::borrow::Cow;
use std::boxed::FnBox;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write;
use std::mem;
use std::option;
use std::path::{self, Component, Path, PathBuf};
use std::rc::Rc;
use std::str;

use futures::sync::mpsc;

use crate::cancellation::{CancellationSource, CancellationToken};
use crate::debug_protocol::*;
use crate::disassembly;
use crate::error::Error;
use crate::expressions;
use crate::handles::{self, Handle, HandleTree};
use crate::must_initialize::{Initialized, MustInitialize, NotInitialized};
use crate::python::{self, PythonValue};
use crate::source_map;
use crate::terminal::Terminal;
use lldb::*;

pub mod tokio;

type AsyncResponder = FnBox(&mut DebugSession) -> Result<ResponseBody, Error>;

#[derive(Hash, Eq, PartialEq, Debug)]
enum FileId {
    Filename(String),
    Disassembly(Handle),
}

#[derive(Debug, Clone)]
enum BreakpointKind {
    Source {
        file_path: String,
        resolved_line: Option<u32>,
        valid_locations: Vec<BreakpointID>,
    },
    Function,
    Assembly {
        address: u64,
        adapter_data: Vec<u8>,
    },
    Exception,
}

#[derive(Debug, Clone)]
struct BreakpointInfo {
    id: BreakpointID,
    kind: BreakpointKind,
    condition: Option<String>,
    log_message: Option<String>,
    ignore_count: u32,
}

enum Container {
    StackFrame(SBFrame),
    Locals(SBFrame),
    Statics(SBFrame),
    Globals(SBFrame),
    Registers(SBFrame),
    Container(SBValue),
}

enum ExprType {
    Native,
    Python,
    Simple,
}

pub struct DebugSession {
    send_message: mpsc::Sender<ProtocolMessage>,
    request_seq: u32,
    shutdown: CancellationSource,
    event_listener: SBListener,
    debugger: MustInitialize<SBDebugger>,
    target: MustInitialize<SBTarget>,
    process: MustInitialize<SBProcess>,
    process_launched: bool,
    on_configuration_done: Option<(u32, Box<AsyncResponder>)>,
    source_breakpoints: HashMap<FileId, HashMap<i64, BreakpointID>>,
    fn_breakpoints: HashMap<String, BreakpointID>,
    breakpoints: RefCell<HashMap<BreakpointID, BreakpointInfo>>,
    var_refs: HandleTree<Container>,
    disassembly: MustInitialize<disassembly::AddressSpace>,
    known_threads: HashSet<ThreadID>,
    source_map: source_map::SourceMap,
    source_map_cache: HashMap<(Cow<'static, str>, Cow<'static, str>), Option<Rc<String>>>,
    loaded_modules: Vec<SBModule>,
    exit_commands: Option<Vec<String>>,
    terminal: Option<Terminal>,

    global_format: Format,
    show_disassembly: Option<bool>,
    suppress_missing_files: bool,
    deref_pointers: bool,
    container_summary: bool,
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////

unsafe impl Send for DebugSession {}

impl Drop for DebugSession {
    fn drop(&mut self) {
        info!("### Dropping DebugSession");
    }
}

impl DebugSession {
    fn new(send_message: mpsc::Sender<ProtocolMessage>, shutdown: CancellationSource) -> Self {
        DebugSession {
            send_message: send_message,
            request_seq: 1,
            shutdown: shutdown,
            debugger: NotInitialized,
            target: NotInitialized,
            process: NotInitialized,
            process_launched: false,
            event_listener: SBListener::new_with_name("DebugSession"),
            on_configuration_done: None,
            source_breakpoints: HashMap::new(),
            fn_breakpoints: HashMap::new(),
            breakpoints: RefCell::new(HashMap::new()),
            var_refs: HandleTree::new(),
            disassembly: NotInitialized,
            known_threads: HashSet::new(),
            source_map: source_map::SourceMap::empty(),
            source_map_cache: HashMap::new(),
            loaded_modules: Vec::new(),
            exit_commands: None,
            terminal: None,

            global_format: Format::Default,
            show_disassembly: None,
            suppress_missing_files: true,
            deref_pointers: true,
            container_summary: true,
        }
    }

    fn handle_message(&mut self, message: ProtocolMessage) {
        match message {
            ProtocolMessage::Request(request) => self.handle_request(request),
            ProtocolMessage::Response(response) => self.handle_response(response),
            ProtocolMessage::Event(event) => error!("No handler for event message: {:?}", event),
        };
    }

    fn handle_response(&mut self, response: Response) {}

    fn handle_request(&mut self, request: Request) {
        let result = if let Some(arguments) = request.arguments {
            #[cfg_attr(rustfmt, rustfmt_skip)]
            match arguments {
                RequestArguments::initialize(args) =>
                    self.handle_initialize(args)
                        .map(|r| ResponseBody::initialize(r)),
                RequestArguments::setBreakpoints(args) =>
                    self.handle_set_breakpoints(args)
                        .map(|r| ResponseBody::setBreakpoints(r)),
                RequestArguments::setFunctionBreakpoints(args) =>
                    self.handle_set_function_breakpoints(args)
                        .map(|r| ResponseBody::setFunctionBreakpoints(r)),
                RequestArguments::setExceptionBreakpoints(args) =>
                    self.handle_set_exception_breakpoints(args)
                        .map(|r| ResponseBody::setExceptionBreakpoints),
                RequestArguments::launch(args) => {
                    match self.handle_launch(args) {
                        Ok(responder) => {
                            self.on_configuration_done = Some((request.seq, responder));
                            return; // launch responds asynchronously
                        }
                        Err(err) => Err(err),
                    }
                }
                RequestArguments::attach(args) => {
                    match self.handle_attach(args) {
                        Ok(responder) => {
                            self.on_configuration_done = Some((request.seq, responder));
                            return; // attach responds asynchronously
                        }
                        Err(err) => Err(err),
                    }
                }
                RequestArguments::configurationDone =>
                    self.handle_configuration_done()
                        .map(|r| ResponseBody::configurationDone),
                RequestArguments::threads =>
                    self.handle_threads()
                        .map(|r| ResponseBody::threads(r)),
                RequestArguments::stackTrace(args) =>
                    self.handle_stack_trace(args)
                        .map(|r| ResponseBody::stackTrace(r)),
                RequestArguments::scopes(args) =>
                    self.handle_scopes(args)
                        .map(|r| ResponseBody::scopes(r)),
                RequestArguments::variables(args) =>
                    self.handle_variables(args)
                        .map(|r| ResponseBody::variables(r)),
                RequestArguments::evaluate(args) =>
                    self.handle_evaluate(args)
                        .map(|r| ResponseBody::evaluate(r)),
                RequestArguments::pause(args) =>
                    self.handle_pause(args)
                        .map(|_| ResponseBody::pause),
                RequestArguments::continue_(args) =>
                    self.handle_continue(args)
                        .map(|r| ResponseBody::continue_(r)),
                RequestArguments::next(args) =>
                    self.handle_next(args)
                        .map(|r| ResponseBody::next),
                RequestArguments::stepIn(args) =>
                    self.handle_step_in(args)
                        .map(|r| ResponseBody::stepIn),
                RequestArguments::stepOut(args) =>
                    self.handle_step_out(args)
                        .map(|r| ResponseBody::stepOut),
                RequestArguments::source(args) =>
                    self.handle_source(args)
                        .map(|r| ResponseBody::source(r)),
                RequestArguments::disconnect(args) =>
                    self.handle_disconnect(Some(args))
                        .map(|_| ResponseBody::disconnect),
                RequestArguments::displaySettings(args) =>
                    self.handle_display_settings(args)
                        .map(|_| ResponseBody::displaySettings),
                _ => {
                    //error!("No handler for request message: {:?}", request);
                    Err(Error::Internal("Not implemented.".into()))
                }
            }
        } else {
            self.handle_disconnect(None).map(|_| ResponseBody::disconnect)
        };
        info!("sending result {:?}", result);
        self.send_response(request.seq, result);
    }

    fn send_response(&mut self, request_seq: u32, result: Result<ResponseBody, Error>) {
        let response = match result {
            Ok(body) => ProtocolMessage::Response(Response {
                request_seq: request_seq,
                success: true,
                message: None,
                body: Some(body),
            }),
            Err(err) => {
                error!("{}", err);
                ProtocolMessage::Response(Response {
                    request_seq: request_seq,
                    success: false,
                    body: None,
                    message: Some(format!("{}", err)),
                })
            }
        };
        self.send_message
            .try_send(response)
            .map_err(|err| panic!("Could not send response: {}", err));
    }

    fn send_event(&mut self, event_body: EventBody) {
        let event = ProtocolMessage::Event(Event {
            seq: 0,
            body: event_body,
        });
        self.send_message
            .try_send(event)
            .map_err(|err| panic!("Could not send event: {}", err));
    }

    fn send_request(&mut self, args: RequestArguments) {
        let request = ProtocolMessage::Request(Request {
            seq: self.request_seq,
            arguments: Some(args),
        });
        self.request_seq += 1;
        self.send_message
            .try_send(request)
            .map_err(|err| panic!("Could not send request: {}", err));
    }

    fn console_message(&mut self, output: impl Into<String>) {
        self.send_event(EventBody::output(OutputEventBody {
            output: format!("{}\n", output.into()),
            ..Default::default()
        }));
    }

    fn console_error(&mut self, output: impl Into<String>) {
        self.send_event(EventBody::output(OutputEventBody {
            output: format!("{}\n", output.into()),
            category: Some("stderr".into()),
            ..Default::default()
        }));
    }

    fn handle_initialize(&mut self, args: InitializeRequestArguments) -> Result<Capabilities, Error> {
        self.debugger = Initialized(SBDebugger::create(false));
        self.debugger.set_async(true);
        python::initialize(&self.debugger.command_interpreter());
        let mut command_result = SBCommandReturnObject::new();

        let caps = Capabilities {
            supports_configuration_done_request: true,
            supports_evaluate_for_hovers: false, // TODO
            supports_function_breakpoints: true,
            supports_conditional_breakpoints: true,
            supports_hit_conditional_breakpoints: true,
            supports_set_variable: true,
            supports_completions_request: false, // TODO
            supports_delayed_stack_trace_loading: true,
            support_terminate_debuggee: true,
            supports_log_points: true,
        };
        Ok(caps)
    }

    fn handle_set_breakpoints(&mut self, args: SetBreakpointsArguments) -> Result<SetBreakpointsResponseBody, Error> {
        let file_id = FileId::Filename(args.source.path.as_ref()?.clone());

        let requested_bps = args.breakpoints.as_ref().unwrap();
        let mut old_existing_bps = self.source_breakpoints.remove(&file_id).unwrap_or_default();

        let mut existing_bps = HashMap::new();
        for (line, bp_id) in old_existing_bps.drain() {
            if !requested_bps.iter().any(|rbp| rbp.line == line) {
                self.target.breakpoint_delete(bp_id);
                self.breakpoints.borrow_mut().remove(&bp_id);
            } else {
                existing_bps.insert(line, bp_id);
            }
        }

        let breakpoints = self.set_source_breakpoints(
            &mut existing_bps,
            &args.breakpoints.as_ref()?,
            args.source.path.as_ref()?,
        );

        self.source_breakpoints.insert(file_id, existing_bps);

        let response = SetBreakpointsResponseBody { breakpoints };
        Ok(response)
    }

    fn set_source_breakpoints(
        &mut self, existing_bps: &mut HashMap<i64, BreakpointID>, requested_bps: &[SourceBreakpoint], file_path: &str,
    ) -> Vec<Breakpoint> {
        let mut breakpoints_resp = vec![];
        let mut breakpoints = self.breakpoints.borrow_mut();
        for req in requested_bps {
            let mut bp_resp = Breakpoint { ..Default::default() };

            let (mut bp, bp_info) = if let Some(bp_id) = existing_bps.get(&req.line).cloned() {
                let bp = self.target.find_breakpoint_by_id(bp_id);
                let bp_info = breakpoints.get_mut(&bp_id).unwrap();
                bp_resp.id = Some(bp.id() as i64);
                bp_resp.verified = true;
                (bp, bp_info)
            } else {
                let file_name = Path::new(file_path).file_name().unwrap().to_str().unwrap();
                let bp = self.target.breakpoint_create_by_location(file_name, req.line as u32);

                let mut bp_info = BreakpointInfo {
                    id: bp.id(),
                    kind: BreakpointKind::Source {
                        file_path: file_path.to_owned(),
                        resolved_line: None,
                        valid_locations: vec![],
                    },
                    condition: None,
                    log_message: None,
                    ignore_count: 0,
                };

                let bp_id = bp_info.id;
                existing_bps.insert(req.line, bp_info.id);
                bp_resp.id = Some(bp_info.id as i64);

                // Filter locations on full source file path
                for bp_loc in bp.locations() {
                    if !self.is_valid_source_bp_location(&bp_loc, &mut bp_info) {
                        bp_loc.set_enabled(false);
                        //info!("Disabled BP location {}", bp_loc);
                    }
                }

                if let BreakpointKind::Source {
                    resolved_line: Some(line),
                    ..
                } = bp_info.kind
                {
                    bp_resp.verified = true;
                    bp_resp.line = Some(line as i64);
                    bp_resp.source = Some(Source {
                        name: Some(file_name.to_owned()),
                        path: Some(file_path.to_owned()),
                        adapter_data: Some(json!(bp_info.id)),
                        ..Default::default()
                    })
                }
                let bp_info = breakpoints.entry(bp_info.id).or_insert(bp_info);
                (bp, bp_info)
            };
            bp_info.condition = req.condition.clone();
            bp_info.log_message = req.log_message.clone();

            self.init_bp_actions(&mut bp, bp_info);

            breakpoints_resp.push(bp_resp);
        }
        breakpoints_resp
    }

    fn handle_set_function_breakpoints(
        &mut self, args: SetFunctionBreakpointsArguments,
    ) -> Result<SetBreakpointsResponseBody, Error> {
        let mut breakpoints_resp = vec![];
        let mut new_fn_breakpoints = HashMap::new();

        let mut breakpoints = self.breakpoints.borrow_mut();
        for bp_req in args.breakpoints {
            let (mut bp, bp_info) = match self.fn_breakpoints.get(&bp_req.name) {
                Some(bp_id) => {
                    let bp = self.target.find_breakpoint_by_id(*bp_id);
                    let bp_info = breakpoints.get_mut(bp_id).unwrap();
                    (bp, bp_info)
                }
                None => {
                    let bp = if bp_req.name.starts_with("/re ") {
                        self.target.breakpoint_create_by_regex(&bp_req.name[4..])
                    } else {
                        self.target.breakpoint_create_by_name(&bp_req.name)
                    };
                    let bp_info = BreakpointInfo {
                        id: bp.id(),
                        kind: BreakpointKind::Function,
                        condition: None,
                        log_message: None,
                        ignore_count: 0,
                    };
                    let bp_info = breakpoints.entry(bp_info.id).or_insert(bp_info);
                    (bp, bp_info)
                }
            };
            bp_info.condition = bp_req.condition;

            let bp_id = bp_info.id;
            self.init_bp_actions(&mut bp, bp_info);

            new_fn_breakpoints.insert(bp_req.name, bp_id);
            breakpoints_resp.push(Breakpoint {
                id: Some(bp_id as i64),
                verified: bp.num_resolved_locations() > 0,
                ..Default::default()
            });
        }

        // Delete existing breakpoints that are not in new_fn_breakpoints
        for (name, bp_id) in &self.fn_breakpoints {
            if !new_fn_breakpoints.contains_key(name) {
                self.target.breakpoint_delete(*bp_id);
            }
        }
        self.fn_breakpoints = new_fn_breakpoints;

        let response = SetBreakpointsResponseBody {
            breakpoints: breakpoints_resp,
        };
        Ok(response)
    }

    fn handle_set_exception_breakpoints(&mut self, args: SetExceptionBreakpointsArguments) -> Result<(), Error> {
        Ok(())
    }

    fn init_bp_actions(&self, bp: &mut SBBreakpoint, bp_info: &BreakpointInfo) {
        fn evaluate_python_bp_condition(
            expr: &str, process: &SBProcess, thread: &SBThread, location: &SBBreakpointLocation,
        ) -> bool {
            let debugger = process.target().debugger();
            let interpreter = debugger.command_interpreter();
            let context = SBExecutionContext::from_frame(&thread.frame_at_index(0));
            match python::evaluate(&interpreter, &expr, true, &context) {
                Err(_) => true, // Stop on evluation errors
                Ok(val) => match val {
                    PythonValue::SBValue(val) => match val.try_value_as_unsigned() {
                        Ok(val) => val != 0,
                        Err(_) => true,
                    },
                    PythonValue::Bool(val) => val,
                    _ => true,
                },
            }
        }

        if let Some(ref condition) = bp_info.condition {
            let (expr, ty) = self.get_expression_type(condition);
            match ty {
                ExprType::Native => bp.set_condition(expr),
                ExprType::Simple => {
                    let pp_expr = expressions::preprocess_simple_expr(expr);
                    bp.set_callback(move |process, thread, location| {
                        evaluate_python_bp_condition(&pp_expr, process, thread, location)
                    });
                }
                ExprType::Python => {
                    let pp_expr = expressions::preprocess_python_expr(expr);
                    bp.set_callback(move |process, thread, location| {
                        evaluate_python_bp_condition(&pp_expr, process, thread, location)
                    });
                }
            }
        } else {
            bp.clear_callback();
        }
        // TODO: hit count & log_message
    }

    fn is_valid_source_bp_location(&self, bp_loc: &SBBreakpointLocation, bp_info: &mut BreakpointInfo) -> bool {
        if let Some(le) = bp_loc.address().line_entry() {
            if let BreakpointKind::Source {
                ref mut resolved_line, ..
            } = bp_info.kind
            {
                *resolved_line = Some(le.line());
            }
        }
        true
    }

    fn handle_launch(&mut self, args: LaunchRequestArguments) -> Result<Box<AsyncResponder>, Error> {
        if let Some(commands) = &args.init_commands {
            self.exec_commands(&commands);
        }
        self.target = Initialized(self.create_target(&args.program)?);
        self.disassembly = Initialized(disassembly::AddressSpace::new(&self.target));
        self.send_event(EventBody::initialized);
        Ok(Box::new(move |s: &mut DebugSession| s.complete_launch(args)))
    }

    fn complete_launch(&mut self, args: LaunchRequestArguments) -> Result<ResponseBody, Error> {
        if let Some(ref commands) = args.pre_run_commands {
            self.exec_commands(commands);
        }
        let mut launch_info = SBLaunchInfo::new();

        // TODO: Streaming iterator?
        let env: Vec<String> = env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();
        launch_info.set_environment_entries(env.iter().map(|s| s.as_ref()), true);

        if let Some(ref ds) = args.display_settings {
            self.update_display_settings(ds);
        }
        if let Some(ref args) = args.args {
            launch_info.set_arguments(args.iter().map(|a| a.as_ref()), false);
        }
        if let Some(ref env) = args.env {
            // TODO: Streaming iterator?
            let env: Vec<String> = env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            launch_info.set_environment_entries(env.iter().map(|s| s.as_ref()), true);
        }
        if let Some(ref cwd) = args.cwd {
            launch_info.set_working_directory(&cwd);
        }
        if let Some(stop_on_entry) = args.stop_on_entry {
            if stop_on_entry {
                launch_info.set_launch_flags(launch_info.launch_flags() | LaunchFlag::StopAtEntry);
            }
        }
        if let Some(ref source_map) = args.source_map {
            let iter = source_map.iter().map(|(k, v)| (k, v.as_ref()));
            self.source_map = source_map::SourceMap::new(iter)?;
        }

        self.configure_stdio(&args, &mut launch_info);

        //env::set_var("LLDB_DEBUGSERVER_PATH", "/usr/bin/lldb-server");

        launch_info.set_listener(&self.event_listener);

        self.process = Initialized(self.target.launch(&launch_info)?);
        self.process_launched = true;
        if let Some(commands) = args.post_run_commands {
            self.exec_commands(&commands);
        }
        self.exit_commands = args.exit_commands;
        Ok(ResponseBody::launch)
    }

    fn run_in_vscode_terminal(&mut self, terminal_kind: TerminalKind, mut args: Vec<String>) {
        let terminal_kind = match terminal_kind {
            TerminalKind::External => "external",
            TerminalKind::Integrated => {
                args.insert(0, "\n".into());
                "integrated"
            }
            _ => unreachable!(),
        };
        let req_args = RunInTerminalRequestArguments {
            args: args,
            cwd: String::new(),
            env: None,
            kind: Some(terminal_kind.to_owned()),
            title: Some("Debuggee".to_owned()),
        };
        self.send_request(RequestArguments::runInTerminal(req_args));
    }

    fn handle_attach(&mut self, args: AttachRequestArguments) -> Result<Box<AsyncResponder>, Error> {
        unimplemented!()
    }

    fn create_target(&self, program: &str) -> Result<SBTarget, Error> {
        let target = match self.debugger.create_target(program, None, None, false) {
            Ok(target) => target,
            // TODO: use selected platform instead of cfg!(windows)
            Err(_) if cfg!(windows) && !program.ends_with(".exe") => {
                let program = format!("{}.exe", program);
                match self.debugger.create_target(&program, None, None, false) {
                    Ok(target) => target,
                    Err(err) => return Err(err.into()),
                }
            }
            Err(err) => return Err(err.into()),
        };
        target.broadcaster().add_listener(
            &self.event_listener,
            SBTargetEvent::BroadcastBitBreakpointChanged | SBTargetEvent::BroadcastBitModulesLoaded,
        );
        Ok(target)
    }

    fn configure_stdio(&mut self, args: &LaunchRequestArguments, launch_info: &mut SBLaunchInfo) -> Result<(), Error> {
        let tty_name = match args.terminal {
            Some(ref terminal_kind) => {
                if cfg!(unix) {
                    // use selected platform instead of cfg
                    match terminal_kind {
                        TerminalKind::External | TerminalKind::Integrated => {
                            let terminal =
                                Terminal::create(|args| self.run_in_vscode_terminal(terminal_kind.clone(), args))?;
                            let tty_name = terminal.tty_name().to_owned();
                            self.terminal = Some(terminal);
                            Some(tty_name)
                        }
                        TerminalKind::Console => None,
                    }
                } else {
                    // cfg!(windows)
                    let without_console = match terminal_kind {
                        TerminalKind::External => "false",
                        TerminalKind::Integrated | TerminalKind::Console => "true",
                    };
                    env::set_var("LLDB_LAUNCH_INFERIORS_WITHOUT_CONSOLE", without_console);
                    None
                }
            }
            None => None,
        };

        let mut stdio = match args.stdio {
            Some(ref stdio) => stdio.clone(),
            None => vec![],
        };
        // Pad to at least 3 entries
        while stdio.len() < 3 {
            stdio.push(None)
        }

        for (fd, name) in stdio.iter().enumerate() {
            let (read, write) = match fd {
                0 => (true, false),
                1 => (false, true),
                2 => (false, true),
                _ => (true, true),
            };
            let name = name.as_ref().or(tty_name.as_ref());
            if let Some(name) = name {
                launch_info.add_open_file_action(fd as i32, name, read, write);
            }
        }

        Ok(())
    }

    fn exec_commands(&self, commands: &[String]) {
        let interpreter = self.debugger.command_interpreter();
        let mut command_result = SBCommandReturnObject::new();
        for command in commands {
            interpreter.handle_command(&command, &mut command_result, false);
        }
    }

    fn handle_configuration_done(&mut self) -> Result<(), Error> {
        if let Some((request_seq, mut responder)) = self.on_configuration_done.take() {
            let result = responder.call_box((self,));
            self.send_response(request_seq, result);
        }
        Ok(())
    }

    fn handle_threads(&mut self) -> Result<ThreadsResponseBody, Error> {
        let mut response = ThreadsResponseBody { threads: vec![] };
        for thread in self.process.threads() {
            response.threads.push(Thread {
                id: thread.thread_id() as i64,
                name: format!("{}: tid={}", thread.index_id(), thread.thread_id()),
            });
        }
        Ok(response)
    }

    fn handle_stack_trace(&mut self, args: StackTraceArguments) -> Result<StackTraceResponseBody, Error> {
        let thread = self
            .process
            .thread_by_id(args.thread_id as ThreadID)
            .expect("Invalid thread id");

        let start_frame = args.start_frame.unwrap_or(0);
        let levels = args.levels.unwrap_or(std::i64::MAX);

        let mut stack_frames = vec![];
        for i in start_frame..(start_frame + levels) {
            let frame = thread.frame_at_index(i as u32);
            if !frame.is_valid() {
                break;
            }

            let handle = self
                .var_refs
                .create(None, "[frame]", Container::StackFrame(frame.clone()));
            let mut stack_frame: StackFrame = Default::default();

            stack_frame.id = handle.get() as i64;
            let pc_address = frame.pc_address();
            stack_frame.name = if let Some(name) = frame.function_name() {
                name.to_owned()
            } else {
                format!("{:X}", pc_address.file_address())
            };

            if !self.in_disassembly(&frame) {
                if let Some(le) = frame.line_entry() {
                    let fs = le.file_spec();
                    if let Some(local_path) = self.map_filespec_to_local(&fs) {
                        stack_frame.line = le.line() as i64;
                        stack_frame.column = le.column() as i64;
                        stack_frame.source = Some(Source {
                            name: Some(fs.filename().to_owned()),
                            path: Some(local_path.as_ref().clone()),
                            ..Default::default()
                        });
                    }
                }
            } else {
                let pc_addr = frame.pc_address();
                let dasm = match self.disassembly.get_by_address(&pc_addr) {
                    Some(dasm) => dasm,
                    None => {
                        debug!("Creating disassembly for {:?}", pc_addr);
                        self.disassembly.create_from_address(&pc_addr)
                    }
                };
                stack_frame.line = dasm.line_num_by_address(pc_addr.load_address(&self.target)) as i64;
                stack_frame.column = 0;
                stack_frame.source = Some(Source {
                    name: Some(dasm.source_name().to_owned()),
                    source_reference: Some(handles::to_i64(Some(dasm.handle()))),
                    ..Default::default()
                });
            }
            stack_frames.push(stack_frame);
        }

        Ok(StackTraceResponseBody {
            stack_frames: stack_frames,
            total_frames: Some(thread.num_frames() as i64),
        })
    }

    fn in_disassembly(&mut self, frame: &SBFrame) -> bool {
        match self.show_disassembly {
            Some(v) => v,
            None => if let Some(le) = frame.line_entry() {
                self.map_filespec_to_local(&le.file_spec()).is_none()
            } else {
                true
            },
        }
    }

    fn handle_scopes(&mut self, args: ScopesArguments) -> Result<ScopesResponseBody, Error> {
        let frame_id = Handle::new(args.frame_id as u32).unwrap();
        if let Some(Container::StackFrame(frame)) = self.var_refs.get(frame_id) {
            let frame = frame.clone();
            let locals_handle = self
                .var_refs
                .create(Some(frame_id), "[locs]", Container::Locals(frame.clone()));
            let locals = Scope {
                name: "Local".into(),
                variables_reference: locals_handle.get() as i64,
                expensive: false,
                ..Default::default()
            };
            let statics_handle = self
                .var_refs
                .create(Some(frame_id), "[stat]", Container::Statics(frame.clone()));
            let statics = Scope {
                name: "Static".into(),
                variables_reference: statics_handle.get() as i64,
                expensive: false,
                ..Default::default()
            };
            let globals_handle = self
                .var_refs
                .create(Some(frame_id), "[glob]", Container::Globals(frame.clone()));
            let globals = Scope {
                name: "Global".into(),
                variables_reference: globals_handle.get() as i64,
                expensive: false,
                ..Default::default()
            };
            let registers_handle = self
                .var_refs
                .create(Some(frame_id), "[regs]", Container::Registers(frame));
            let registers = Scope {
                name: "Registers".into(),
                variables_reference: registers_handle.get() as i64,
                expensive: false,
                ..Default::default()
            };
            Ok(ScopesResponseBody {
                scopes: vec![locals, statics, globals, registers],
            })
        } else {
            Err(Error::Internal(format!("Invalid frame reference: {}", args.frame_id)))
        }
    }

    fn handle_variables(&mut self, args: VariablesArguments) -> Result<VariablesResponseBody, Error> {
        let container_handle = handles::from_i64(args.variables_reference).unwrap();

        if let Some(container) = self.var_refs.get(container_handle) {
            let variables = match container {
                Container::Locals(frame) => {
                    let ret_val = frame.thread().stop_return_value();
                    let variables = frame.variables(&VariableOptions {
                        arguments: true,
                        locals: true,
                        statics: false,
                        in_scope_only: true,
                        use_dynamic: DynamicValueType::NoDynamicValues,
                    });
                    let mut vars_iter = ret_val.into_iter().chain(variables.iter());
                    self.convert_scope_values(&mut vars_iter, "", Some(container_handle))
                }
                Container::Statics(frame) => {
                    let variables = frame.variables(&VariableOptions {
                        arguments: false,
                        locals: false,
                        statics: true,
                        in_scope_only: true,
                        use_dynamic: DynamicValueType::NoDynamicValues,
                    });
                    let mut vars_iter = variables.iter().filter(|v| v.value_type() != ValueType::VariableStatic);
                    self.convert_scope_values(&mut vars_iter, "", Some(container_handle))
                }
                Container::Globals(frame) => {
                    let variables = frame.variables(&VariableOptions {
                        arguments: false,
                        locals: false,
                        statics: true,
                        in_scope_only: true,
                        use_dynamic: DynamicValueType::NoDynamicValues,
                    });
                    let mut vars_iter = variables.iter(); //.filter(|v| v.value_type() != ValueType::VariableGlobal);
                    self.convert_scope_values(&mut vars_iter, "", Some(container_handle))
                }
                Container::Registers(frame) => {
                    let list = frame.registers();
                    let mut vars_iter = list.iter();
                    self.convert_scope_values(&mut vars_iter, "", Some(container_handle))
                }
                Container::Container(var) => {
                    let container_eval_name = self.compose_container_eval_name(container_handle);
                    let var = var.clone();
                    let mut vars_iter = var.children();
                    let mut variables =
                        self.convert_scope_values(&mut vars_iter, &container_eval_name, Some(container_handle));
                    // If synthetic, add [raw] view.
                    if var.is_synthetic() {
                        let raw_var = var.non_synthetic_value();
                        let handle =
                            self.var_refs
                                .create(Some(container_handle), "[raw]", Container::Container(raw_var));
                        let raw = Variable {
                            name: "[raw]".to_owned(),
                            value: var.type_name().unwrap_or_default().to_owned(),
                            variables_reference: handles::to_i64(Some(handle)),
                            ..Default::default()
                        };
                        variables.push(raw);
                    }
                    variables
                }
                Container::StackFrame(_) => vec![],
            };
            Ok(VariablesResponseBody { variables: variables })
        } else {
            Err(Error::Internal(format!(
                "Invalid variabes reference: {}",
                container_handle
            )))
        }
    }

    fn compose_container_eval_name(&self, container_handle: Handle) -> String {
        let mut eval_name = String::new();
        let mut container_handle = Some(container_handle);
        while let Some(h) = container_handle {
            let (parent_handle, key, value) = self.var_refs.get_full_info(h).unwrap();
            match value {
                Container::Container(var) if var.value_type() != ValueType::RegisterSet => {
                    eval_name = compose_eval_name(key, eval_name);
                    container_handle = parent_handle;
                }
                _ => break,
            }
        }
        eval_name
    }

    fn convert_scope_values(
        &mut self, vars_iter: &mut Iterator<Item = SBValue>, container_eval_name: &str,
        container_handle: Option<Handle>,
    ) -> Vec<Variable> {
        let mut variables = vec![];
        let mut variables_idx = HashMap::new();
        for var in vars_iter {
            let name = var.name().unwrap_or_default();
            let dtype = var.type_name();
            let value = self.get_var_value_str(&var, self.global_format, container_handle.is_some());
            let handle = self.get_var_handle(container_handle, name, &var);

            let eval_name = if var.prefer_synthetic_value() {
                Some(compose_eval_name(container_eval_name, name))
            } else {
                var.expression_path().map(|p| {
                    let mut p = p;
                    p.insert_str(0, "/nat ");
                    p
                })
            };

            let variable = Variable {
                name: name.to_owned(),
                value: value,
                type_: dtype.map(|v| v.to_owned()),
                variables_reference: handles::to_i64(handle),
                evaluate_name: eval_name,
                ..Default::default()
            };

            // Ensure proper shadowing
            if let Some(idx) = variables_idx.get(&variable.name) {
                variables[*idx] = variable;
            } else {
                variables_idx.insert(variable.name.clone(), variables.len());
                variables.push(variable);
            }
        }
        variables
    }

    // Generate a handle for a variable.
    fn get_var_handle(&mut self, parent_handle: Option<Handle>, key: &str, var: &SBValue) -> Option<Handle> {
        if var.num_children() > 0 || var.is_synthetic() {
            Some(
                self.var_refs
                    .create(parent_handle, key, Container::Container(var.clone())),
            )
        } else {
            None
        }
    }

    // Get a displayable string from a SBValue
    fn get_var_value_str(&self, var: &SBValue, format: Format, is_container: bool) -> String {
        // TODO: let mut var: Cow<&SBValue> = var.into(); ???
        let mut value_opt: Option<String> = None;
        let mut var2: Option<SBValue> = None;
        let mut var = var;
        var.set_format(format);

        if self.deref_pointers && format == Format::Default {
            let type_class = var.type_().type_class();
            if type_class.intersects(TypeClass::Pointer | TypeClass::Reference) {
                if var.value_as_unsigned(0) == 0 {
                    value_opt = Some("<null>".to_owned());
                } else {
                    if var.is_synthetic() {
                        value_opt = var.summary().map(|s| into_string_lossy(s));
                    } else {
                        var2 = Some(var.dereference());
                        var = var2.as_ref().unwrap();
                    }
                }
            }
        }

        // Try value, then summary
        if value_opt.is_none() {
            value_opt = var.value().map(|s| into_string_lossy(s));
            if value_opt.is_none() {
                value_opt = var.summary().map(|s| into_string_lossy(s));
            }
        }

        let value_str = match value_opt {
            Some(s) => s,
            None => {
                if is_container {
                    if self.container_summary {
                        self.get_container_summary(var)
                    } else {
                        "{...}".to_owned()
                    }
                } else {
                    "<not available>".to_owned()
                }
            }
        };

        value_str
    }

    fn get_container_summary(&self, var: &SBValue) -> String {
        const MAX_LENGTH: usize = 32;

        let mut summary = String::from("{");
        let mut empty = true;
        for child in var.children() {
            if let Some(name) = child.name() {
                if let Some(Ok(value)) = child.value().map(|s| s.to_str()) {
                    if empty {
                        empty = false;
                    } else {
                        summary.push_str(", ");
                    }

                    if name.starts_with("[") {
                        summary.push_str(value);
                    } else {
                        write!(summary, "{}:{}", name, value);
                    }
                }
            }

            if summary.len() > MAX_LENGTH {
                summary.push_str(", ...");
                break;
            }
        }
        if empty {
            summary.push_str("...");
        }
        summary.push_str("}");
        summary
    }

    fn get_expr_format<'a>(&self, expr: &'a str) -> (&'a str, Option<Format>) {
        let mut chars = expr.chars();
        if let Some(ch) = chars.next_back() {
            if let Some(',') = chars.next_back() {
                let format = match ch {
                    'h' => Format::Hex,
                    'x' => Format::Hex,
                    'o' => Format::Octal,
                    'd' => Format::Decimal,
                    'b' => Format::Binary,
                    'f' => Format::Float,
                    'p' => Format::Pointer,
                    'u' => Format::Unsigned,
                    's' => Format::CString,
                    'y' => Format::Bytes,
                    'Y' => Format::BytesWithASCII,
                    _ => return (expr, None),
                };
                return (chars.as_str(), Some(format));
            }
        }
        (expr, None)
    }

    fn handle_evaluate(&mut self, args: EvaluateArguments) -> Result<EvaluateResponseBody, Error> {
        let frame: Option<&SBFrame> = args.frame_id.map(|id| {
            let handle = handles::from_i64(id).unwrap();
            if let Some(Container::StackFrame(ref frame)) = self.var_refs.get(handle) {
                frame
            } else {
                panic!("Invalid frameId");
            }
        });

        let context = args.context.as_ref().map(|s| s.as_ref());
        let mut expression: &str = &args.expression;

        if let Some("repl") = context {
            if !expression.starts_with("?") {
                // LLDB command
                let result = self.execute_command_in_frame(expression, frame);
                let text = if result.succeeded() {
                    result.output()
                } else {
                    result.error()
                };
                let response = EvaluateResponseBody {
                    result: into_string_lossy(text),
                    ..Default::default()
                };
                return Ok(response);
            } else {
                expression = &expression[1..]; // drop '?'
            }
        }
        // Expression
        let (expression, expr_format) = self.get_expr_format(expression);
        let expr_format = expr_format.unwrap_or(self.global_format);
        self.evaluate_expr_in_frame(expression, frame).map(|val| match val {
            PythonValue::SBValue(sbval) => {
                let handle = self.get_var_handle(None, expression, &sbval);
                EvaluateResponseBody {
                    result: self.get_var_value_str(&sbval, expr_format, handle.is_some()),
                    type_: sbval.type_name().map(|s| s.to_owned()),
                    variables_reference: handles::to_i64(handle),
                    ..Default::default()
                }
            }
            PythonValue::Int(val) => EvaluateResponseBody {
                result: val.to_string(),
                ..Default::default()
            },
            PythonValue::Bool(val) => EvaluateResponseBody {
                result: val.to_string(),
                ..Default::default()
            },
            PythonValue::String(s) | PythonValue::Object(s) => EvaluateResponseBody {
                result: s,
                ..Default::default()
            },
        })
    }

    // Evaluates expr in the context of frame (or in global context if frame is None)
    // Returns expressions.Value or SBValue on success, SBError on failure.
    fn evaluate_expr_in_frame(&self, expr: &str, frame: Option<&SBFrame>) -> Result<PythonValue, Error> {
        let (expr, ty) = self.get_expression_type(expr);
        match ty {
            ExprType::Native => {
                let result = match frame {
                    Some(frame) => frame.evaluate_expression(expr),
                    None => self.target.evaluate_expression(expr),
                };
                let error = result.error();
                if error.is_success() {
                    Ok(PythonValue::SBValue(result))
                } else {
                    Err(error.into())
                }
            }
            ExprType::Python => {
                let pp_expr = expressions::preprocess_python_expr(expr);
                let interpreter = self.debugger.command_interpreter();
                let context = self.context_from_frame(frame);
                match python::evaluate(&interpreter, &pp_expr, false, &context) {
                    Ok(val) => Ok(val),
                    Err(s) => Err(Error::UserError(s)),
                }
            }
            ExprType::Simple => {
                let pp_expr = expressions::preprocess_simple_expr(expr);
                let interpreter = self.debugger.command_interpreter();
                let context = self.context_from_frame(frame);
                match python::evaluate(&interpreter, &pp_expr, true, &context) {
                    Ok(val) => Ok(val),
                    Err(s) => Err(Error::UserError(s)),
                }
            }
        }
    }

    // Classify expression by evaluator type
    fn get_expression_type<'a>(&self, expr: &'a str) -> (&'a str, ExprType) {
        if expr.starts_with("/nat ") {
            (&expr[5..], ExprType::Native)
        } else if expr.starts_with("/py ") {
            (&expr[4..], ExprType::Python)
        } else if expr.starts_with("/se ") {
            (&expr[4..], ExprType::Simple)
        } else {
            // TODO: expressions config
            (expr, ExprType::Simple)
        }
    }

    fn execute_command_in_frame(&self, command: &str, frame: Option<&SBFrame>) -> SBCommandReturnObject {
        let context = self.context_from_frame(frame);
        let mut result = SBCommandReturnObject::new();
        let interp = self.debugger.command_interpreter();
        interp.handle_command_with_context(command, &context, &mut result, false);
        // TODO: multiline
        result
    }

    fn context_from_frame(&self, frame: Option<&SBFrame>) -> SBExecutionContext {
        match frame {
            Some(frame) => SBExecutionContext::from_frame(&frame),
            None => match self.process {
                Initialized(ref process) => {
                    let thread = process.selected_thread();
                    SBExecutionContext::from_thread(&thread)
                }
                NotInitialized => {
                    let target = self.debugger.selected_target();
                    SBExecutionContext::from_target(&target)
                }
            },
        }
    }

    fn handle_pause(&mut self, args: PauseArguments) -> Result<(), Error> {
        let error = self.process.stop();
        if error.is_success() {
            Ok(())
        } else {
            Err(Error::UserError(error.message().into()))
        }
    }

    fn handle_continue(&mut self, args: ContinueArguments) -> Result<ContinueResponseBody, Error> {
        self.before_resume();
        let error = self.process.resume();
        if error.is_success() {
            Ok(ContinueResponseBody {
                all_threads_continued: Some(true),
            })
        } else {
            Err(Error::UserError(error.message().into()))
        }
    }

    fn handle_next(&mut self, args: NextArguments) -> Result<(), Error> {
        self.before_resume();
        let thread = self.process.thread_by_id(args.thread_id as ThreadID)?;
        let frame = thread.frame_at_index(0);
        if !self.in_disassembly(&frame) {
            thread.step_over();
        } else {
            thread.step_instruction(true);
        }
        Ok(())
    }

    fn handle_step_in(&mut self, args: StepInArguments) -> Result<(), Error> {
        self.before_resume();
        let thread = self.process.thread_by_id(args.thread_id as ThreadID)?;
        let frame = thread.frame_at_index(0);
        if !self.in_disassembly(&frame) {
            thread.step_into();
        } else {
            thread.step_instruction(false);
        }
        Ok(())
    }

    fn handle_step_out(&mut self, args: StepOutArguments) -> Result<(), Error> {
        self.before_resume();
        let thread = self.process.thread_by_id(args.thread_id as ThreadID)?;
        thread.step_out();
        Ok(())
    }

    fn handle_source(&mut self, args: SourceArguments) -> Result<SourceResponseBody, Error> {
        let handle = handles::from_i64(args.source_reference).unwrap();
        let dasm = self.disassembly.get_by_handle(handle).unwrap();
        Ok(SourceResponseBody {
            content: dasm.get_source_text(),
            mime_type: Some("text/x-lldb.disassembly".to_owned()),
        })
    }

    fn handle_disconnect(&mut self, args: Option<DisconnectArguments>) -> Result<(), Error> {
        if let Some(commands) = &self.exit_commands {
            self.exec_commands(&commands);
        }
        let terminate = match args {
            None => self.process_launched,
            Some(args) => match args.terminate_debuggee {
                None => self.process_launched,
                Some(terminate) => terminate,
            },
        };
        if let Initialized(ref process) = self.process {
            if terminate {
                process.kill();
            } else {
                process.detach();
            }
        }
        self.shutdown.request_cancellation();
        Ok(())
    }

    fn handle_display_settings(&mut self, args: DisplaySettingsArguments) -> Result<(), Error> {
        self.update_display_settings(&args);
        self.refresh_client_display();
        Ok(())
    }

    fn update_display_settings(&mut self, args: &DisplaySettingsArguments) {
        self.global_format = match args.display_format {
            None => self.global_format,
            Some(DisplayFormat::Auto) => Format::Default,
            Some(DisplayFormat::Decimal) => Format::Decimal,
            Some(DisplayFormat::Hex) => Format::Hex,
            Some(DisplayFormat::Binary) => Format::Binary,
        };
        self.show_disassembly = match args.show_disassembly {
            None => self.show_disassembly,
            Some(ShowDisassembly::Auto) => None,
            Some(ShowDisassembly::Always) => Some(true),
            Some(ShowDisassembly::Never) => Some(false),
        };
        self.deref_pointers = match args.dereference_pointers {
            None => self.deref_pointers,
            Some(v) => v,
        };
        self.container_summary = match args.container_summary {
            None => self.container_summary,
            Some(v) => v,
        };
        // Show current settings
        let show_disasm = match self.show_disassembly {
            None => "auto",
            Some(true) => "always",
            Some(false) => "never",
        };
        let msg = format!("Display settings: variable format={}, show disassembly={}, numeric pointer values={}, container summaries={}.",
            format!("{:?}", self.global_format).to_lowercase(),
            show_disasm,
            if self.deref_pointers { "on" } else { "off" },
            if self.container_summary { "on" } else { "off" }
        );
        self.console_message(msg);
    }

    // Fake target start/stop to force VSCode to refresh UI state.
    fn refresh_client_display(&mut self) {
        let thread_id = self.process.selected_thread().thread_id();
        self.send_event(EventBody::continued(ContinuedEventBody {
            thread_id: thread_id as i64,
            all_threads_continued: Some(true),
        }));
        self.send_event(EventBody::stopped(StoppedEventBody {
            thread_id: Some(thread_id as i64),
            //preserve_focus_hint: Some(true),
            all_threads_stopped: Some(true),
            ..Default::default()
        }));
    }

    fn before_resume(&mut self) {
        self.var_refs.reset();
    }

    fn handle_debug_event(&mut self, event: SBEvent) {
        debug!("Debug event: {:?}", event);
        if let Some(process_event) = event.as_process_event() {
            self.handle_process_event(&process_event);
        } else if let Some(target_event) = event.as_target_event() {
            self.handle_target_event(&target_event);
        } else if let Some(bp_event) = event.as_breakpoint_event() {
            self.handle_breakpoint_event(&bp_event);
        }
    }

    fn handle_process_event(&mut self, process_event: &SBProcessEvent) {
        let flags = process_event.as_event().flags();
        if flags & SBProcessEvent::BroadcastBitStateChanged != 0 {
            match process_event.process_state() {
                ProcessState::Running => self.send_event(EventBody::continued(ContinuedEventBody {
                    all_threads_continued: Some(true),
                    thread_id: 0,
                })),
                ProcessState::Stopped if !process_event.restarted() => self.notify_process_stopped(&process_event),
                ProcessState::Crashed => self.notify_process_stopped(&process_event),
                ProcessState::Exited => {
                    let exit_code = self.process.exit_status() as i64;
                    self.send_event(EventBody::exited(ExitedEventBody { exit_code }));
                    self.send_event(EventBody::terminated(TerminatedEventBody { restart: None }));
                }
                ProcessState::Detached => self.send_event(EventBody::terminated(TerminatedEventBody { restart: None })),
                _ => (),
            }
        }
    }

    fn notify_process_stopped(&mut self, event: &SBProcessEvent) {
        self.update_threads();
        // Find thread that has caused this stop
        let mut stopped_thread = None;
        // Check the currently selected thread first
        let selected_thread = self.process.selected_thread();
        stopped_thread = match selected_thread.stop_reason() {
            StopReason::Invalid | // br
            StopReason::None => None,
            _ => Some(selected_thread),
        };
        // Fall back to scanning all threads in the process
        if stopped_thread.is_none() {
            for thread in self.process.threads() {
                match thread.stop_reason() {
                    StopReason::Invalid | // br
                    StopReason::None => (),
                    _ => {
                        self.process.set_selected_thread(&thread);
                        stopped_thread = Some(thread);
                        break;
                    }
                }
            }
        }
        // Analyze stop reason
        let (stop_reason_str, description) = match stopped_thread {
            Some(ref stopped_thread) => {
                let stop_reason = stopped_thread.stop_reason();
                match stop_reason {
                    StopReason::Breakpoint => ("breakpoint", None),
                    StopReason::Trace | // br
                    StopReason::PlanComplete => ("step", None),
                    _ => {
                        // Print stop details for these types
                        let description = Some(stopped_thread.stop_description());
                        match stop_reason {
                            StopReason::Watchpoint => ("watchpoint", description),
                            StopReason::Signal => ("signal", description),
                            StopReason::Exception => ("exception", description),
                            _ => ("unknown", description),
                        }
                    }
                }
            }
            None => ("unknown", None),
        };

        self.send_event(EventBody::stopped(StoppedEventBody {
            all_threads_stopped: Some(true),
            description: None,
            preserve_focus_hint: None,
            reason: stop_reason_str.to_owned(),
            text: description,
            thread_id: stopped_thread.map(|t| t.thread_id() as i64),
        }));

        let interpreter = self.debugger.command_interpreter();
        python::modules_loaded(&interpreter, &mut self.loaded_modules.iter());
        self.loaded_modules.clear();
    }

    // Notify VSCode about target threads that started or exited since the last stop.
    fn update_threads(&mut self) {
        let threads = self.process.threads().map(|t| t.thread_id()).collect::<HashSet<_>>();
        let started = threads.difference(&self.known_threads).cloned().collect::<Vec<_>>();
        let exited = self.known_threads.difference(&threads).cloned().collect::<Vec<_>>();
        for tid in exited {
            self.send_event(EventBody::thread(ThreadEventBody {
                thread_id: tid as i64,
                reason: "exited".to_owned(),
            }));
        }
        for tid in started {
            self.send_event(EventBody::thread(ThreadEventBody {
                thread_id: tid as i64,
                reason: "started".to_owned(),
            }));
        }
        self.known_threads = threads;
    }

    fn handle_target_event(&mut self, event: &SBTargetEvent) {
        let flags = event.as_event().flags();
        if flags & SBTargetEvent::BroadcastBitModulesLoaded != 0 {
            // Running scripts during target execution seems to trigger a bug in LLDB,
            // so we defer loaded module notification till the next stop.
            for module in event.modules() {
                self.loaded_modules.push(module);
            }
        }
    }

    fn handle_breakpoint_event(&mut self, event: &SBBreakpointEvent) {
        let bp = event.breakpoint();
        let event_type = event.event_type();
        if event_type.intersects(BreakpointEventType::Removed) {
            bp.clear_callback();
        }
    }

    fn map_filespec_to_local(&mut self, filespec: &SBFileSpec) -> Option<Rc<String>> {
        if !filespec.is_valid() {
            return None;
        } else {
            let directory = filespec.directory();
            let filename = filespec.filename();
            match self.source_map_cache.get(&(directory.into(), filename.into())) {
                Some(localized) => localized.clone(),
                None => {
                    debug!("filespec={:?}", filespec);
                    let mut localized = self.source_map.to_local(filespec.path());
                    if let Some(ref path) = localized {
                        if self.suppress_missing_files && !path.is_file() {
                            localized = None;
                        }
                    }
                    let localized = localized.map(|path| Rc::new(path.to_string_lossy().into_owned()));
                    self.source_map_cache.insert(
                        (directory.to_owned().into(), filename.to_owned().into()),
                        localized.clone(),
                    );
                    localized
                }
            }
        }
    }
}

fn compose_eval_name<'a, 'b, A, B>(prefix: A, suffix: B) -> String
where
    A: Into<Cow<'a, str>>,
    B: Into<Cow<'b, str>>,
{
    let prefix = prefix.into();
    let suffix = suffix.into();
    if prefix.as_ref().is_empty() {
        suffix.into_owned()
    } else if suffix.as_ref().is_empty() {
        prefix.into_owned()
    } else if suffix.as_ref().starts_with("[") {
        (prefix + suffix).into_owned()
    } else {
        (prefix + "." + suffix).into_owned()
    }
}

fn into_string_lossy(cstr: &std::ffi::CStr) -> String {
    cstr.to_string_lossy().into_owned()
}

fn opt_as_ref<'a>(x: &'a Option<String>) -> Option<&'a str> {
    x.as_ref().map(|r| r.as_ref())
}
