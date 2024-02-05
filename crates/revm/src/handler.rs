// Modules.
mod handle_types;
pub mod mainnet;
pub mod register;

// Exports.
pub use handle_types::*;

// Includes.
use crate::{
    interpreter::{
        opcode::{make_instruction_table, InstructionTables},
        Host,
    },
    primitives::{db::Database, spec_to_generic, HandlerCfg, Spec, SpecId},
    Evm,
};
use alloc::vec::Vec;
use register::{EvmHandler, HandleRegisters};

/// Handler acts as a proxy and allow to define different behavior for different
/// sections of the code. This allows nice integration of different chains or
/// to disable some mainnet behavior.
pub struct Handler<'a, H: Host + 'a, EXT, DB: Database> {
    /// Handler config.
    pub cfg: HandlerCfg,
    /// Instruction table type.
    pub instruction_table: Option<InstructionTables<'a, H>>,
    /// Registers that will be called on initialization.
    pub registers: Vec<HandleRegisters<'a, EXT, DB>>,
    /// Validity handles.
    pub validation: ValidationHandler<'a, EXT, DB>,
    /// Pre execution handle
    pub pre_execution: PreExecutionHandler<'a, EXT, DB>,
    /// post Execution handle
    pub post_execution: PostExecutionHandler<'a, EXT, DB>,
    /// Execution loop that handles frames.
    pub execution: ExecutionHandler<'a, EXT, DB>,
}

impl<'a, EXT, DB: Database + 'a> EvmHandler<'a, EXT, DB> {
    /// Created new Handler with given configuration.
    ///
    /// Internaly it calls `mainnet_with_spec` with the given spec id.
    /// Or `optimism_with_spec` if the optimism feature is enabled and `cfg.is_optimism` is set.
    pub fn new(cfg: HandlerCfg) -> Self {
        cfg_if::cfg_if! {
            if #[cfg(feature = "optimism")] {
                if cfg.is_optimism {
                    Handler::optimism_with_spec(cfg.spec_id)
                } else {
                    Handler::mainnet_with_spec(cfg.spec_id)
                }
            } else {
                Handler::mainnet_with_spec(cfg.spec_id)
            }
        }
    }
    /// Handler for the mainnet
    pub fn mainnet<SPEC: Spec + 'static>() -> Self {
        Self {
            cfg: HandlerCfg::new(SPEC::SPEC_ID),
            instruction_table: Some(InstructionTables::Plain(make_instruction_table::<
                Evm<'a, EXT, DB>,
                SPEC,
            >())),
            registers: Vec::new(),
            validation: ValidationHandler::new::<SPEC>(),
            pre_execution: PreExecutionHandler::new::<SPEC>(),
            post_execution: PostExecutionHandler::new::<SPEC>(),
            execution: ExecutionHandler::new::<SPEC>(),
        }
    }

    /// Is optimism enabled.
    pub fn is_optimism(&self) -> bool {
        self.cfg.is_optimism()
    }

    /// Handler for optimism
    #[cfg(feature = "optimism")]
    pub fn optimism<SPEC: Spec + 'static>() -> Self {
        let mut handler = Self::mainnet::<SPEC>();
        handler.cfg.is_optimism = true;
        handler.append_handle_register(HandleRegisters::Plain(
            crate::optimism::optimism_handle_register::<DB, EXT>,
        ));
        handler
    }

    /// Optimism with spec. Similar to [`Self::mainnet_with_spec`]
    #[cfg(feature = "optimism")]
    pub fn optimism_with_spec(spec_id: SpecId) -> Self {
        spec_to_generic!(spec_id, Self::optimism::<SPEC>())
    }

    /// Creates handler with variable spec id, inside it will call `mainnet::<SPEC>` for
    /// appropriate spec.
    pub fn mainnet_with_spec(spec_id: SpecId) -> Self {
        spec_to_generic!(spec_id, Self::mainnet::<SPEC>())
    }

    /// Specification ID.
    pub fn cfg(&self) -> HandlerCfg {
        self.cfg
    }

    /// Take instruction table.
    pub fn take_instruction_table(&mut self) -> Option<InstructionTables<'a, Evm<'a, EXT, DB>>> {
        self.instruction_table.take()
    }

    /// Set instruction table.
    pub fn set_instruction_table(&mut self, table: InstructionTables<'a, Evm<'a, EXT, DB>>) {
        self.instruction_table = Some(table);
    }

    /// Returns reference to pre execution handler.
    pub fn pre_execution(&self) -> &PreExecutionHandler<'a, EXT, DB> {
        &self.pre_execution
    }

    /// Returns reference to pre execution handler.
    pub fn post_execution(&self) -> &PostExecutionHandler<'a, EXT, DB> {
        &self.post_execution
    }

    /// Returns reference to frame handler.
    pub fn execution(&self) -> &ExecutionHandler<'a, EXT, DB> {
        &self.execution
    }

    /// Returns reference to validation handler.
    pub fn validation(&self) -> &ValidationHandler<'a, EXT, DB> {
        &self.validation
    }

    /// Append handle register.
    pub fn append_handle_register(&mut self, register: HandleRegisters<'a, EXT, DB>) {
        register.register(self);
        self.registers.push(register);
    }

    /// Creates the Handler with Generic Spec.
    pub fn create_handle_generic<SPEC: Spec + 'static>(&mut self) -> EvmHandler<'a, EXT, DB> {
        let registers = core::mem::take(&mut self.registers);
        let mut base_handler = Handler::mainnet::<SPEC>();
        // apply all registers to default handeler and raw mainnet instruction table.
        for register in registers {
            base_handler.append_handle_register(register)
        }
        base_handler
    }

    /// Creates the Handler with variable SpecId, inside it will call function with Generic Spec.
    pub fn change_spec_id(mut self, spec_id: SpecId) -> EvmHandler<'a, EXT, DB> {
        if self.cfg.spec_id == spec_id {
            return self;
        }

        let registers = core::mem::take(&mut self.registers);
        // register for optimism is added as a register, so we need to create mainnet handler here.
        let mut handler = Handler::mainnet_with_spec(spec_id);
        // apply all registers to default handeler and raw mainnet instruction table.
        for register in registers {
            handler.append_handle_register(register)
        }
        handler.cfg = self.cfg();
        handler
    }
}
