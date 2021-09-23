pub mod config;

use config::Config;

use rust_debug::call_stack::{CallFrame, MemoryAccess};
use rust_debug::evaluate::evaluate::{get_udata, EvaluatorValue};
use rust_debug::registers::Registers;
use rust_debug::source_information::{find_breakpoint_location, SourceInformation};

use gimli::DebugFrame;
use gimli::Dwarf;
use gimli::Reader;

use super::commands::{
    debug_event::DebugEvent, debug_request::DebugRequest, debug_response::DebugResponse, Command,
};

use super::Opt;
use super::{attach_probe, read_dwarf};
use anyhow::{anyhow, Context, Result};
use capstone::arch::BuildsCapstone;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use debugserver_types::{Breakpoint, SourceBreakpoint};
use log::{info, warn};
use probe_rs::flashing::{download_file, Format};
use probe_rs::{CoreStatus, MemoryInterface};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct DebugHandler {
    config: Config,
}

impl DebugHandler {
    pub fn new(opt: Opt) -> DebugHandler {
        DebugHandler {
            config: Config::new(opt),
        }
    }

    pub fn new_default() -> DebugHandler {
        DebugHandler {
            config: Config {
                elf_file_path: None,
                chip: None,
                work_directory: None,
                probe_num: 0,
            },
        }
    }

    pub fn run(
        &mut self,
        mut sender: Sender<Command>,
        mut reciver: Receiver<DebugRequest>,
    ) -> Result<()> {
        loop {
            let request = reciver.recv()?;
            let (exit, response) = match self.handle_request(&mut sender, &mut reciver, request) {
                Ok(val) => val,
                Err(err) => {
                    sender.send(Command::Response(DebugResponse::Error {
                        message: format!("{:?}", err),
                    }))?;
                    continue;
                }
            };
            sender.send(Command::Response(response))?;

            if exit {
                return Ok(());
            }
        }
    }

    fn handle_request(
        &mut self,
        sender: &mut Sender<Command>,
        reciver: &mut Receiver<DebugRequest>,
        request: DebugRequest,
    ) -> Result<(bool, DebugResponse)> {
        match request {
            DebugRequest::Exit => Ok((true, DebugResponse::Exit)),
            DebugRequest::SetBinary { path } => {
                self.config.elf_file_path = Some(path);
                Ok((false, DebugResponse::SetBinary))
            }
            DebugRequest::SetProbeNumber { number } => {
                self.config.probe_num = number;
                Ok((false, DebugResponse::SetProbeNumber))
            }
            DebugRequest::SetChip { chip } => {
                self.config.chip = Some(chip);
                Ok((false, DebugResponse::SetChip))
            }
            DebugRequest::SetCWD { cwd } => {
                self.config.work_directory = Some(cwd);
                Ok((false, DebugResponse::SetCWD))
            }
            _ => {
                if self.config.is_missing_config() {
                    return Ok((
                        false,
                        DebugResponse::Error {
                            message: self.config.missing_config_message(),
                        },
                    ));
                }

                let new_request = init(
                    sender,
                    reciver,
                    self.config.elf_file_path.clone().unwrap(),
                    self.config.probe_num,
                    self.config.chip.clone().unwrap(),
                    self.config.work_directory.clone().unwrap(),
                    request,
                )?;
                self.handle_request(sender, reciver, new_request)
            }
        }
    }
}

pub fn init(
    sender: &mut Sender<Command>,
    reciver: &mut Receiver<DebugRequest>,
    file_path: PathBuf,
    probe_number: usize,
    chip: String,
    cwd: String,
    request: DebugRequest,
) -> Result<DebugRequest> {
    let cs = capstone::Capstone::new() // TODO: Set the capstone base on the arch of the chip.
        .arm()
        .mode(capstone::arch::arm::ArchMode::Thumb)
        .build()
        .expect("Failed to create Capstone object");

    let (owned_dwarf, owned_debug_frame) = read_dwarf(&file_path)?;
    let debug_info = DebugInformation::new(&owned_dwarf, &owned_debug_frame);

    let mut session = attach_probe(&chip, probe_number)?;

    let (pc_reg, link_reg, sp_reg) = {
        let core = session.core(0)?;
        let pc_reg =
            probe_rs::CoreRegisterAddress::from(core.registers().program_counter()).0 as usize;
        let link_reg =
            probe_rs::CoreRegisterAddress::from(core.registers().return_address()).0 as usize;
        let sp_reg =
            probe_rs::CoreRegisterAddress::from(core.registers().stack_pointer()).0 as usize;
        (pc_reg, link_reg, sp_reg)
    };
    let mut registers = Registers::new();
    registers.program_counter_register = Some(pc_reg);
    registers.link_register = Some(link_reg);
    registers.stack_pointer_register = Some(sp_reg);

    let mut debugger = Debugger {
        capstone: cs,
        debug_info,
        session,
        breakpoints: HashMap::new(),
        file_path,
        cwd,
        check_time: Instant::now(),
        running: true,
        registers,
        stack_trace: None,
    };

    debugger.run(sender, reciver, request)
}

struct Debugger<'a, R: Reader<Offset = usize>> {
    debug_info: DebugInformation<'a, R>,
    session: probe_rs::Session,
    capstone: capstone::Capstone,
    breakpoints: HashMap<u32, Breakpoint>,
    file_path: PathBuf,
    cwd: String,
    check_time: Instant,
    running: bool,
    registers: Registers,
    stack_trace: Option<Vec<StackFrame>>,
}

impl<'a, R: Reader<Offset = usize>> Debugger<'a, R> {
    pub fn run(
        &mut self,
        sender: &mut Sender<Command>,
        reciver: &mut Receiver<DebugRequest>,
        request: DebugRequest,
    ) -> Result<DebugRequest> {
        match self.handle_request(request)? {
            Command::Request(req) => return Ok(req),
            Command::Response(res) => sender.send(Command::Response(res))?,
            _ => unimplemented!(),
        };

        loop {
            match reciver.try_recv() {
                Ok(request) => {
                    match self.handle_request(request)? {
                        Command::Request(req) => {
                            let mut core = self.session.core(0)?;
                            core.clear_all_hw_breakpoints()?;
                            self.breakpoints = HashMap::new();

                            return Ok(req);
                        }
                        Command::Response(res) => sender.send(Command::Response(res))?,
                        _ => unimplemented!(),
                    };
                }
                Err(err) => {
                    match err {
                        TryRecvError::Empty => self.check_halted(sender)?,
                        TryRecvError::Disconnected => {
                            let mut core = self.session.core(0)?;
                            core.clear_all_hw_breakpoints()?;
                            self.breakpoints = HashMap::new();

                            return Err(anyhow!("{:?}", err));
                        }
                    };
                }
            };
        }
    }

    fn clear_temporaries(&mut self) {
        self.registers.clear();
        self.stack_trace = None;
    }

    fn check_halted(&mut self, sender: &mut Sender<Command>) -> Result<()> {
        let delta = Duration::from_millis(400);
        if self.running && self.check_time.elapsed() > delta {
            self.check_time = Instant::now();
            self.send_halt_event(sender)?;
        }

        Ok(())
    }

    fn send_halt_event(&mut self, sender: &mut Sender<Command>) -> Result<()> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;

        if let CoreStatus::Halted(reason) = status {
            self.running = false;

            let pc = core.read_core_reg(core.registers().program_counter())?;

            let mut hit_breakpoint_ids = vec![];
            match self.breakpoints.get(&pc) {
                Some(bkpt) => hit_breakpoint_ids.push(bkpt.id.unwrap() as u32),
                None => (),
            };

            sender.send(Command::Event(DebugEvent::Halted {
                pc: pc,
                reason: reason,
                hit_breakpoint_ids: Some(hit_breakpoint_ids),
            }))?;
        }

        Ok(())
    }

    fn handle_request(&mut self, request: DebugRequest) -> Result<Command> {
        match request {
            DebugRequest::Attach {
                reset,
                reset_and_halt,
            } => self.attach_command(reset, reset_and_halt),
            DebugRequest::Stack => self.stack_command(),
            DebugRequest::Code => self.code_command(),
            DebugRequest::ClearAllBreakpoints => self.clear_all_breakpoints_command(),
            DebugRequest::ClearBreakpoint { address } => self.clear_breakpoint_command(address),
            DebugRequest::SetBreakpoint {
                address,
                source_file,
            } => self.set_breakpoint_command(address, source_file),
            DebugRequest::Registers => self.registers_command(),
            DebugRequest::Variable { name } => self.variable_command(&name),
            DebugRequest::Variables => self.variables_command(),
            DebugRequest::StackTrace => self.stack_trace_command(),
            DebugRequest::Read { address, byte_size } => self.read_command(address, byte_size),
            DebugRequest::Reset {
                reset_and_halt: rah,
            } => self.reset_command(rah),
            DebugRequest::Flash {
                reset_and_halt: rah,
            } => self.flash_command(rah),
            DebugRequest::Halt => self.halt_command(),
            DebugRequest::Status => self.status_command(),
            DebugRequest::Continue => self.continue_command(),
            DebugRequest::Step => self.step_command(),
            DebugRequest::SetBreakpoints {
                source_file,
                source_breakpoints,
            } => self.set_breakpoints_command(source_file, source_breakpoints),

            _ => Ok(Command::Request(request)),
        }
    }

    fn attach_command(&mut self, reset: bool, reset_and_halt: bool) -> Result<Command> {
        if reset_and_halt {
            self.clear_temporaries();
            let mut core = self.session.core(0)?;
            core.reset_and_halt(std::time::Duration::from_millis(10))
                .context("Failed to reset and halt the core")?;
        } else if reset {
            self.clear_temporaries();
            let mut core = self.session.core(0)?;
            core.reset().context("Failed to reset the core")?;
        }

        Ok(Command::Response(DebugResponse::Attach))
    }

    fn stack_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;

        if status.is_halted() {
            let sp_reg: u16 =
                probe_rs::CoreRegisterAddress::from(core.registers().stack_pointer()).0;

            let sf = core.read_core_reg(7)?; // reg 7 seams to be the base stack address.
            let sp = core.read_core_reg(sp_reg)?;

            if sf < sp {
                // The previous stack pointer is less then current.
                // This happens when there is no stack.
                return Ok(Command::Response(DebugResponse::Stack {
                    stack_pointer: sp,
                    stack: vec![],
                }));
            }

            let length = (((sf - sp) + 4 - 1) / 4) as usize;
            let mut stack = vec![0u32; length];
            core.read_32(sp, &mut stack)?;

            return Ok(Command::Response(DebugResponse::Stack {
                stack_pointer: sp,
                stack: stack,
            }));
        } else {
            return Err(anyhow!("Core must be halted"));
        }
    }

    fn code_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;

        if status.is_halted() {
            let pc = core.registers().program_counter();
            let pc_val = core.read_core_reg(pc)?;

            let mut code = [0u8; 16 * 2];

            core.read_8(pc_val, &mut code)?;

            let insns = self
                .capstone
                .disasm_all(&code, pc_val as u64)
                .expect("Failed to disassemble");

            let mut instructions = vec![];
            for i in insns.iter() {
                instructions.push((i.address() as u32, i.to_string()));
            }

            return Ok(Command::Response(DebugResponse::Code {
                pc: pc_val,
                instructions: instructions,
            }));
        } else {
            warn!("Core is not halted, status: {:?}", status);
            return Err(anyhow!("Core must be halted"));
        }
    }

    fn clear_all_breakpoints_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        core.clear_all_hw_breakpoints()?;
        self.breakpoints = HashMap::new();

        info!("All breakpoints cleared");

        Ok(Command::Response(DebugResponse::ClearAllBreakpoints))
    }

    fn clear_breakpoint_command(&mut self, address: u32) -> Result<Command> {
        let mut core = self.session.core(0)?;

        match self.breakpoints.remove(&address) {
            Some(_bkpt) => {
                core.clear_hw_breakpoint(address)?;
                info!("Breakpoint cleared from: 0x{:08x}", address);
                Ok(Command::Response(DebugResponse::ClearBreakpoint))
            }
            None => {
                core.clear_hw_breakpoint(address)?;
                Err(anyhow!("Can't remove hardware breakpoint at {}", address))
            }
        }
    }

    fn set_breakpoint_command(
        &mut self,
        mut address: u32,
        source_file: Option<String>,
    ) -> Result<Command> {
        let mut core = self.session.core(0)?;
        address = match source_file {
            Some(path) => find_breakpoint_location(
                self.debug_info.dwarf,
                &self.cwd,
                &path,
                address as u64,
                None,
            )?
            .expect("Could not file location form source file line number")
                as u32,
            None => address,
        };

        let num_bkpt = self.breakpoints.len() as u32;
        let tot_bkpt = core.get_available_breakpoint_units()?;

        if num_bkpt < tot_bkpt {
            core.set_hw_breakpoint(address)?;

            let breakpoint = Breakpoint {
                id: Some(address as i64),
                verified: true,
                message: None,
                source: None, // TODO
                line: None,   // TODO
                column: None, // TODO
                end_line: None,
                end_column: None,
            };
            let _bkpt = self.breakpoints.insert(address, breakpoint);

            info!("Breakpoint set at: 0x{:08x}", address);
            return Ok(Command::Response(DebugResponse::SetBreakpoint));
        } else {
            return Err(anyhow!("All hardware breakpoints are already set"));
        }
    }

    fn registers_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let register_file = core.registers();

        let mut registers = vec![];
        for register in register_file.registers() {
            let value = core.read_core_reg(register)?;

            registers.push((format!("{}", register.name()), value));
        }

        Ok(Command::Response(DebugResponse::Registers { registers }))
    }

    fn variable_command(&mut self, name: &str) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;
        drop(core);

        match status.is_halted() {
            true => match &self.stack_trace {
                Some(stack_trace) => {
                    if stack_trace.len() < 1 {
                        return Err(anyhow!("Variable {:?} not found", name));
                    }
                    let variable = match stack_trace[0].find_variable(name) {
                        Some(var) => var.clone(),
                        None => {
                            return Ok(Command::Response(DebugResponse::Error {
                                message: format!("Variable {:?} not found", name),
                            }))
                        }
                    };

                    Ok(Command::Response(DebugResponse::Variable { variable }))
                }
                None => {
                    self.set_stack_trace()?;
                    self.variable_command(name)
                }
            },
            false => Err(anyhow!("Core must be halted")),
        }
    }

    fn variables_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;
        drop(core);

        match status.is_halted() {
            true => match &self.stack_trace {
                Some(stack_trace) => {
                    let variables = match stack_trace.len() {
                        0 => vec![],
                        _ => stack_trace[0].variables.clone(),
                    };

                    Ok(Command::Response(DebugResponse::Variables {
                        variables: variables,
                    }))
                }
                None => {
                    self.set_stack_trace()?;
                    self.variables_command()
                }
            },
            false => Err(anyhow!("Core must be halted")),
        }
    }

    fn stack_trace_command(&mut self) -> Result<Command> {
        match &self.stack_trace {
            Some(stack_trace) => Ok(Command::Response(DebugResponse::StackTrace {
                stack_trace: stack_trace.clone(),
            })),
            None => {
                self.set_stack_trace()?;
                self.stack_trace_command()
            }
        }
    }

    fn read_command(&mut self, address: u32, byte_size: usize) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let mut buff: Vec<u8> = vec![0; byte_size];
        core.read_8(address, &mut buff)?;

        Ok(Command::Response(DebugResponse::Read {
            address: address,
            value: buff,
        }))
    }

    fn reset_command(&mut self, reset_and_halt: bool) -> Result<Command> {
        if reset_and_halt {
            self.clear_temporaries();

            let mut core = self.session.core(0)?;
            core.reset_and_halt(std::time::Duration::from_millis(10))
                .context("Failed to reset and halt the core")?;
        } else {
            self.clear_temporaries();

            let mut core = self.session.core(0)?;
            core.reset().context("Failed to reset the core")?;
        }

        self.running = true;

        Ok(Command::Response(DebugResponse::Reset))
    }

    fn flash_command(&mut self, reset_and_halt: bool) -> Result<Command> {
        download_file(&mut self.session, &self.file_path, Format::Elf)
            .context("Failed to flash target")?;

        if reset_and_halt {
            self.clear_temporaries();

            let mut core = self.session.core(0)?;
            core.reset_and_halt(std::time::Duration::from_millis(10))
                .context("Failed to reset and halt the core")?;
        } else {
            self.clear_temporaries();

            let mut core = self.session.core(0)?;
            core.reset().context("Failed to reset the core")?;
        }

        self.running = true;

        Ok(Command::Response(DebugResponse::Flash))
    }

    fn halt_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;

        if status.is_halted() {
            warn!("Core is already halted, status: {:?}", status);
            return Err(anyhow!("Core is already halted"));
        } else {
            let cpu_info = core.halt(Duration::from_millis(100))?;
            info!("Core halted at pc = 0x{:08x}", cpu_info.pc);
        };

        Ok(Command::Response(DebugResponse::Halt))
    }

    fn status_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;
        let mut pc = None;

        if status.is_halted() {
            pc = Some(core.read_core_reg(core.registers().program_counter())?);
        }

        Ok(Command::Response(DebugResponse::Status {
            status: status,
            pc: pc,
        }))
    }

    fn step_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let status = core.status()?;

        if status.is_halted() {
            let pc = continue_fix(&mut core, &self.breakpoints)?;
            self.running = true;
            info!("Stopped at pc = 0x{:08x}", pc);

            drop(core);

            self.clear_temporaries();
            return Ok(Command::Response(DebugResponse::Step));
        }

        Ok(Command::Response(DebugResponse::Error {
            message: "Can only step when core is halted".to_owned(),
        }))
    }

    fn continue_command(&mut self) -> Result<Command> {
        let mut core = self.session.core(0)?;
        let mut status = core.status()?;

        if status.is_halted() {
            let _pc = continue_fix(&mut core, &self.breakpoints)?;
            core.run()?;
            self.running = true;
            status = core.status()?;

            drop(core);

            self.clear_temporaries();
        }

        info!("Core status: {:?}", status);

        Ok(Command::Response(DebugResponse::Continue))
    }

    fn set_breakpoints_command(
        &mut self,
        source_file: String,
        source_breakpoints: Vec<SourceBreakpoint>,
    ) -> Result<Command> {
        // Clear all existing breakpoints
        let mut core = self.session.core(0)?;
        core.clear_all_hw_breakpoints()?;
        self.breakpoints = HashMap::new();

        let mut breakpoints = vec![];
        for bkpt in source_breakpoints {
            let breakpoint = match find_breakpoint_location(
                self.debug_info.dwarf,
                &self.cwd,
                &source_file,
                bkpt.line as u64,
                bkpt.column.map(|c| c as u64),
            )? {
                Some(address) => {
                    let mut breakpoint = Breakpoint {
                        id: Some(address as i64),
                        verified: true,
                        message: None,
                        source: None,
                        line: Some(bkpt.line),
                        column: bkpt.column,
                        end_line: None,
                        end_column: None,
                    };

                    // Set breakpoint
                    if self.breakpoints.len() < core.get_available_breakpoint_units()? as usize {
                        self.breakpoints.insert(address as u32, breakpoint.clone());
                        core.set_hw_breakpoint(address as u32)?;
                    } else {
                        breakpoint.verified = false;
                    }

                    breakpoint
                }
                None => Breakpoint {
                    id: None,
                    verified: false,
                    message: None,
                    source: None,
                    line: Some(bkpt.line),
                    column: bkpt.column,
                    end_line: None,
                    end_column: None,
                },
            };

            breakpoints.push(breakpoint);
        }

        Ok(Command::Response(DebugResponse::SetBreakpoints {
            breakpoints,
        }))
    }

    fn set_stack_trace(&mut self) -> Result<()> {
        let mut core = self.session.core(0)?;
        let mut my_core = MyCore { core };

        read_and_add_registers(&mut my_core.core, &mut self.registers)?;
        let stack_trace = rust_debug::call_stack::stack_trace(
            self.debug_info.dwarf,
            self.debug_info.debug_frame,
            self.registers.clone(),
            &mut my_core,
            &self.cwd,
        )?;
        self.stack_trace = Some(resolve_stack_trace(stack_trace)?);

        Ok(())
    }
}

fn continue_fix(
    core: &mut probe_rs::Core,
    breakpoints: &HashMap<u32, Breakpoint>,
) -> Result<u32, probe_rs::Error> {
    match core.status()? {
        probe_rs::CoreStatus::Halted(r) => {
            match r {
                probe_rs::HaltReason::Breakpoint => {
                    let pc = core.registers().program_counter();
                    let pc_val = core.read_core_reg(pc)?;

                    let mut code = [0u8; 2];
                    core.read_8(pc_val, &mut code)?;
                    if code[1] == 190 && code[0] == 0 {
                        // bkpt == 0xbe00 for coretex-m // TODO: is the code[0] == 0 needed?
                        // NOTE: Increment with 2 because bkpt is 2 byte instruction.
                        let step_pc = pc_val + 0x2; // TODO: Fix for other CPU types.
                        core.write_core_reg(pc.into(), step_pc)?;

                        return Ok(step_pc);
                    } else {
                        match breakpoints.get(&pc_val) {
                            Some(_bkpt) => {
                                core.clear_hw_breakpoint(pc_val)?;
                                let pc = core.step()?.pc;
                                core.set_hw_breakpoint(pc_val)?;
                                return Ok(pc);
                            }
                            None => (),
                        };
                    }
                }
                _ => (),
            };
        }
        _ => (),
    };

    Ok(core.step()?.pc)
}

pub struct MyCore<'a> {
    pub core: probe_rs::Core<'a>,
}

impl MemoryAccess for MyCore<'_> {
    fn get_address(&mut self, address: &u32, num_bytes: usize) -> Option<Vec<u8>> {
        let mut buff = vec![0u8; num_bytes];
        match self.core.read_8(*address, &mut buff) {
            Ok(_) => (),
            Err(_) => return None,
        };
        Some(buff)
    }
}

fn read_and_add_registers(core: &mut probe_rs::Core, registers: &mut Registers) -> Result<()> {
    let register_file = core.registers();
    for register in register_file.registers() {
        let value = core.read_core_reg(register)?;
        registers.add_register_value(probe_rs::CoreRegisterAddress::from(register).0, value);
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub struct DebugInformation<'a, R: Reader<Offset = usize>> {
    pub dwarf: &'a Dwarf<R>,
    pub debug_frame: &'a DebugFrame<R>,
    pub breakpoints: Vec<u32>,
}

impl<'a, R: Reader<Offset = usize>> DebugInformation<'a, R> {
    pub fn new(dwarf: &'a Dwarf<R>, debug_frame: &'a DebugFrame<R>) -> DebugInformation<'a, R> {
        DebugInformation {
            dwarf,
            debug_frame,
            breakpoints: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub name: Option<String>,
    pub value: String,
    pub type_: String,
    pub source: Option<SourceInformation>,
    pub children: Vec<Variable>,
}

impl Variable {
    pub fn resolve_varialbe<R: Reader<Offset = usize>>(
        var: &rust_debug::variable::Variable<R>,
    ) -> Result<Variable> {
        let mut variable = Variable {
            name: var.name.clone(),
            value: "This should be overwritten with the correct value".to_string(),
            type_: "".to_owned(),
            source: var.source.clone(),
            children: vec![],
        };

        variable.evaluate(&var.value, &var.source)?;

        return Ok(variable);
    }

    fn evaluate<R: Reader<Offset = usize>>(
        &mut self,
        value: &EvaluatorValue<R>,
        source: &Option<SourceInformation>,
    ) -> Result<()> {
        match value {
            EvaluatorValue::Value(val, _) => {
                self.value = format!("{}", val);
                self.type_ = format!("{}::{}", self.type_, val.get_type());
            }
            EvaluatorValue::PointerTypeValue(pointer_type) => {
                match &pointer_type.name {
                    Some(name) => self.type_ = format!("{}::{}", self.type_, name),
                    None => (),
                };
                self.evaluate(&pointer_type.value, source)?;
            }
            EvaluatorValue::VariantValue(variant_value) => {
                let mut variable = Variable {
                    name: None, // Some("< Variant >".to_owned()),
                    value: match variant_value.discr_value {
                        Some(val) => format!("{}", val),
                        None => "< OptimizedOut >".to_owned(),
                    },
                    type_: "u64".to_string(),
                    source: source.clone(),
                    children: vec![],
                };
                variable.evaluate(
                    &EvaluatorValue::Member(Box::new(variant_value.child.clone())),
                    source,
                )?;
                self.children.push(variable);
            }
            EvaluatorValue::VariantPartValue(variant_part) => {
                match &variant_part.variant {
                    Some(variant) => {
                        self.evaluate(&EvaluatorValue::Member(Box::new(variant.clone())), source)?;
                        let mut child = self.children.pop().ok_or(anyhow!("Error"))?;
                        match &child.name {
                            Some(_) => (),
                            None => child.name = Some("< Variant >".to_owned()),
                        };
                        self.children.push(child);
                    }
                    None => {
                        //let variable = Variable {
                        //    name: Some("< Variant >".to_owned()),
                        //    value: "< OptimizedOut >".to_owned(),
                        //    type_: "u64".to_string(),
                        //    source: source.clone(),
                        //    children: vec![],
                        //};
                        //self.children.push(variable);
                    }
                };
                for variant_value in &variant_part.variants {
                    self.evaluate(
                        &EvaluatorValue::VariantValue(Box::new(variant_value.clone())),
                        source,
                    )?;
                }
            }
            EvaluatorValue::SubrangeTypeValue(subrange_type_value) => {
                match subrange_type_value.count {
                    Some(count) => {
                        let variable = Variable {
                            name: Some("< Length >".to_owned()),
                            value: format!("{}", count),
                            type_: "u64".to_owned(),
                            source: source.clone(),
                            children: vec![],
                        };
                        self.children.push(variable);
                    }
                    None => {
                        match subrange_type_value.base_type_value.clone() {
                            Some((base_type_value, loc)) => {
                                let mut variable = Variable {
                                    name: Some("< Length >".to_owned()),
                                    value: "".to_owned(),
                                    type_: "".to_owned(),
                                    source: source.clone(),
                                    children: vec![],
                                };
                                variable.evaluate(
                                    &EvaluatorValue::<R>::Value(base_type_value, loc),
                                    source,
                                )?;
                                self.children.push(variable);
                            }
                            None => {
                                //let variable = Variable {
                                //    name: Some("< Length >".to_owned()),
                                //    value: "< OptimizedOut >".to_owned(),
                                //    type_: "u64".to_owned(),
                                //    source: source.clone(),
                                //    children: vec![],
                                //};
                                //self.children.push(variable);
                            }
                        };
                    }
                };
            }
            EvaluatorValue::Bytes(bt) => {
                self.value = format!("{:?}", bt);
                self.type_ = format!("{}::{}", self.type_, "< Bytes >");
            }
            EvaluatorValue::Array(array_type_value) => {
                self.value = "".to_owned();
                self.evaluate(
                    &EvaluatorValue::<R>::SubrangeTypeValue(
                        array_type_value.subrange_type_value.clone(),
                    ),
                    source,
                )?;
                for i in 0..array_type_value.values.len() {
                    let mut variable = Variable {
                        name: Some(format!("__{}", i)),
                        value: "< OptimizedOut >".to_owned(),
                        type_: "".to_owned(),
                        source: source.clone(),
                        children: vec![],
                    };
                    variable.evaluate(&array_type_value.values[i], source)?;
                    self.children.push(variable);
                }
            }
            EvaluatorValue::Struct(structure_type_value) => {
                self.type_ = format!("{}::{}", self.type_, structure_type_value.name.clone());
                self.value = structure_type_value.name.clone();

                for member in &structure_type_value.members {
                    self.evaluate(member, source)?;
                }
            }
            EvaluatorValue::Enum(enumeration_type_value) => {
                self.name = Some(enumeration_type_value.name.clone());
                self.type_ = format!("{}::{}", self.type_, enumeration_type_value.name.clone());
                self.value = "< OptimizedOut >".to_owned();
                match &enumeration_type_value.variant {
                    EvaluatorValue::Value(base_type_value, _) => {
                        let variant = get_udata(base_type_value.clone());
                        for enu in &enumeration_type_value.enumerators {
                            if enu.const_value == variant {
                                match &enu.name {
                                    Some(name) => self.value = name.clone(),
                                    None => (),
                                };
                            }
                        }
                    }
                    _ => unimplemented!(),
                };
            }
            EvaluatorValue::Union(union_type_value) => {
                self.type_ = format!("{}::{}", self.type_, union_type_value.name);
                for member in &union_type_value.members {
                    self.evaluate(member, source)?;
                }
            }
            EvaluatorValue::Member(member_value) => {
                let mut variable = Variable {
                    name: member_value.name.clone(),
                    value: "< OptimizedOut >".to_owned(),
                    type_: "".to_owned(),
                    source: source.clone(),
                    children: vec![],
                };
                variable.evaluate(&member_value.value, source)?;
                self.children.push(variable);
            }
            EvaluatorValue::OptimizedOut => self.value = "< OptimizedOut >".to_string(),
            EvaluatorValue::LocationOutOfRange => self.value = "< LocationOutOfRange >".to_string(),
            EvaluatorValue::ZeroSize => self.value = "< OptimizedOut >".to_string(),
        };
        return Ok(());
    }
}

#[derive(Debug, Clone)]
pub struct StackFrame {
    pub name: String,
    pub call_frame: CallFrame,
    pub source: SourceInformation,
    pub variables: Vec<Variable>,
}

impl StackFrame {
    pub fn resolve_stackframe<R: Reader<Offset = usize>>(
        frame: &rust_debug::call_stack::StackFrame<R>,
    ) -> Result<StackFrame> {
        let mut variables = vec![];
        for var in &frame.variables {
            variables.push(Variable::resolve_varialbe(var)?);
        }

        Ok(StackFrame {
            name: frame.name.clone(),
            call_frame: frame.call_frame.clone(),
            source: frame.source.clone(),
            variables,
        })
    }

    pub fn find_variable(&self, name: &str) -> Option<&Variable> {
        for v in &self.variables {
            match &v.name {
                Some(var_name) => {
                    if *var_name == name {
                        return Some(v);
                    }
                }
                None => (),
            };
        }
        return None;
    }
}

pub fn resolve_stack_trace<R: Reader<Offset = usize>>(
    stack_frames: Vec<rust_debug::call_stack::StackFrame<R>>,
) -> Result<Vec<StackFrame>> {
    let mut stack_trace = vec![];
    for sf in &stack_frames {
        stack_trace.push(StackFrame::resolve_stackframe(sf)?);
    }
    Ok(stack_trace)
}
