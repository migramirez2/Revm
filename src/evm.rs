use crate::{
    collection::{vec::Vec, Map},
    inspector::NoOpInspector,
    opcode::Control,
    precompiles, AccountInfo, ExitRevert, Inspector,
};
use core::cmp::min;
use primitive_types::{H160, H256, U256};
use sha3::{Digest, Keccak256};

use super::precompiles::{
    Precompile, PrecompileFn, PrecompileOutput, PrecompileResult, Precompiles,
};
use crate::{
    db::Database,
    error::{ExitError, ExitReason, ExitSucceed},
    machine::{Contract, Gas, Machine, Memory, Stack},
    opcode::OpCode,
    spec::{NotStaticSpec, Spec},
    subroutine::{Account, State, SubRoutine},
    util, CallContext, CreateScheme, GlobalEnv, Log, Transfer,
};
use bytes::Bytes;

pub struct EVM<'a, DB: Database> {
    db: &'a mut DB,
    global_env: GlobalEnv,
    subroutine: SubRoutine,
    precompiles: Precompiles,
    inspector: Box<dyn Inspector>,
}

impl<'a, DB: Database> EVM<'a, DB> {
    pub fn new(db: &'a mut DB, global_env: GlobalEnv) -> Self {
        Self {
            db,
            global_env,
            subroutine: SubRoutine::new(Map::new()), //precompiles::accounts()),
            precompiles: Precompiles::new(),
            inspector: Box::new(NoOpInspector()),
        }
    }

    pub fn inspector(mut self, inspector: Box<dyn Inspector>) -> EVM<'a, DB> {
        self.inspector = inspector;
        self
    }

    fn finalize(
        &mut self,
        caller: H160,
        used_gas_sum: u64,
    ) -> Result<Map<H160, Account>, ExitReason> {
        let eth_spend = U256::from(used_gas_sum) * self.global_env.gas_price;
        let coinbase = self.global_env.block_coinbase;
        // all checks are done prior to this call, so we are safe to transfer without checking outcome.
        let _ = self
            .subroutine
            .transfer(caller, coinbase, eth_spend, self.db);
        let mut out = self.subroutine.finalize();
        let acc = out.get_mut(&caller).unwrap();
        //acc.info.balance += eth_refunded;
        Ok(out)
    }

    fn inner_load_account(&mut self, caller: H160) -> bool {
        let is_cold = self.subroutine.load_account(caller, self.db);
        if is_cold {
            self.inspector.load_account(&caller);
        }
        is_cold
    }

    pub fn call<SPEC: Spec>(
        &mut self,
        caller: H160,
        address: H160,
        value: U256,
        data: Bytes,
        gas_limit: u64,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> (ExitReason, Bytes, u64, State) {
        let gas_used_init = self.initialization::<SPEC>(&data, false, access_list);
        if gas_limit < gas_used_init {
            return (
                ExitReason::Error(ExitError::OutOfGas),
                Bytes::new(),
                0,
                State::default(),
            );
        }

        self.load_account(caller);
        self.subroutine.inc_nonce(caller);

        let context = CallContext {
            caller,
            address,
            apparent_value: value,
        };

        let (exit_reason, gas, bytes) = self.call_inner::<SPEC>(
            address,
            Some(Transfer {
                source: caller,
                target: address,
                value,
            }),
            data,
            gas_limit - gas_used_init,
            context,
        );

        let gas_spend = match exit_reason {
            ExitReason::Succeed(_) => {
                let mut gas_spend = gas.all_used() + gas_used_init;
                let refund_amt = min(gas.refunded() as u64, gas_spend / 2); // for london constant is 5 not 2.
                gas_spend -= refund_amt;
                gas_spend
            }
            ExitReason::Revert(_) => gas.all_used() + gas_used_init,
            _ => gas_limit,
        };

        match self.finalize(caller, gas_spend) {
            //TODO check if refunded can be negative :)
            Err(e) => (e, bytes, gas_spend, Map::new()),
            Ok(state) => (exit_reason, bytes, gas_spend, state),
        }
    }

    pub fn create<SPEC: Spec + NotStaticSpec>(
        &mut self,
        caller: H160,
        value: U256,
        init_code: Bytes,
        create_scheme: CreateScheme,
        gas_limit: u64,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> (ExitReason, Option<H160>, u64, State) {
        let gas_used_init = self.initialization::<SPEC>(&init_code, true, access_list);
        if gas_limit < gas_used_init {
            return (
                ExitReason::Error(ExitError::OutOfGas),
                None,
                0,
                State::default(),
            );
        }

        let (exit_reason, address, gas, _) = self.create_inner::<SPEC>(
            caller,
            create_scheme,
            value,
            init_code,
            gas_limit - gas_used_init,
        );

        let gas_spend = match exit_reason {
            ExitReason::Succeed(_) => {
                let mut gas_spend = gas.all_used() + gas_used_init;
                let refund_amt = min(gas.refunded() as u64, gas_spend / 2); // for london constant is 5 not 2.
                gas_spend -= refund_amt;
                gas_spend
            }
            ExitReason::Revert(_) => gas.all_used() + gas_used_init,
            _ => gas_limit,
        };

        match self.finalize(caller, gas_spend) {
            Err(e) => (e, address, gas_spend, Map::new()),
            Ok(state) => (exit_reason, address, gas_spend, state),
        }
    }

    fn initialization<SPEC: Spec>(
        &mut self,
        input: &Bytes,
        is_create: bool,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> u64 {
        self.precompiles = SPEC::precompiles();
        for &ward_acc in self.precompiles.addresses().iter() {
            //TODO trace load precompiles?
            self.subroutine.load_account(ward_acc, self.db);
        }

        let zero_data_len = input.iter().filter(|v| **v == 0).count() as u64;
        let non_zero_data_len = (input.len() as u64 - zero_data_len) as u64;
        let accessed_accounts = access_list.len() as u64;
        let mut accessed_slots = 0 as u64;

        for (address, slots) in access_list {
            //TODO trace load access_list?
            self.subroutine.load_account(address, self.db);
            accessed_slots += slots.len() as u64;
            for slot in slots {
                self.subroutine.sload(address, slot, self.db);
            }
        }

        let transact = if is_create {
            SPEC::GAS_TRANSACTION_CREATE
        } else {
            SPEC::GAS_TRANSACTION_CALL
        };

        transact
            + zero_data_len * SPEC::GAS_TRANSACTION_ZERO_DATA
            + non_zero_data_len * SPEC::GAS_TRANSACTION_NON_ZERO_DATA
            + accessed_accounts * SPEC::GAS_ACCESS_LIST_ADDRESS
            + accessed_slots * SPEC::GAS_ACCESS_LIST_STORAGE_KEY
    }

    fn create_inner<SPEC: Spec>(
        &mut self,
        caller: H160,
        scheme: CreateScheme,
        value: U256,
        init_code: Bytes,
        gas_limit: u64,
    ) -> (ExitReason, Option<H160>, Gas, Bytes) {
        //println!("create depth:{}",self.subroutine.depth());
        let gas = Gas::new(gas_limit);
        self.load_account(caller);

        // check depth of calls
        if self.subroutine.depth() > SPEC::CALL_STACK_LIMIT {
            return (ExitRevert::CallTooDeep.into(), None, gas, Bytes::new());
        }
        // check balance of caller and value
        if self.balance(caller).0 < value {
            return (ExitRevert::OutOfFund.into(), None, gas, Bytes::new());
        }
        // inc nonce of caller
        let old_nonce = self.subroutine.inc_nonce(caller);
        // create address
        let code_hash = H256::from_slice(Keccak256::digest(&init_code).as_slice());
        let created_address = match scheme {
            CreateScheme::Create => util::create_address(caller, old_nonce),
            CreateScheme::Create2 { salt } => util::create2_address(caller, code_hash, salt),
        };
        let ret = Some(created_address);

        // load account so that it will be hot
        self.load_account(created_address);

        // enter into subroutine
        let checkpoint = self.subroutine.create_checkpoint();

        // create contract account and check for collision
        if !self
            .subroutine
            .new_contract_acc(created_address, self.precompiles.addresses(), self.db)
        {
            self.subroutine.checkpoint_revert(checkpoint);
            return (ExitError::CreateCollision.into(), ret, gas, Bytes::new());
        }

        // transfer value to contract address
        if let Err(e) = self
            .subroutine
            .transfer(caller, created_address, value, self.db)
        {
            self.subroutine.checkpoint_revert(checkpoint);
            return (e.into(), ret, gas, Bytes::new());
        }
        // inc nonce of contract
        if SPEC::CREATE_INCREASE_NONCE {
            self.subroutine.inc_nonce(created_address);
        }
        // create new machine and execute init function
        let contract = Contract::new(Bytes::new(), init_code, created_address, caller, value);
        let mut machine = Machine::new::<SPEC>(contract, gas.limit(), self.subroutine.depth());
        let exit_reason = machine.run::<Self, SPEC>(self);
        // handler error if present on execution\
        match exit_reason {
            ExitReason::Succeed(_) => {
                let b = Bytes::new();
                // if ok, check contract creation limit and calculate gas deduction on output len.
                let code = machine.return_value();
                if let Some(limit) = SPEC::CREATE_CONTRACT_LIMIT {
                    if code.len() > limit {
                        // TODO reduce gas and return
                        self.subroutine.checkpoint_revert(checkpoint);
                        return (ExitError::CreateContractLimit.into(), ret, machine.gas, b);
                    }
                }
                let gas_for_code = code.len() as u64 * crate::opcode::gas::CODEDEPOSIT;
                // record code deposit gas cost and check if we are out of gas.
                if !machine.gas.record_cost(gas_for_code) {
                    self.subroutine.checkpoint_revert(checkpoint);
                    (ExitError::OutOfGas.into(), ret, machine.gas, b)
                } else {
                    // if we have enought gas
                    self.subroutine.checkpoint_commit();
                    self.subroutine.set_code(created_address, code, code_hash);
                    (ExitSucceed::Returned.into(), ret, machine.gas, b)
                }
            }
            _ => {
                self.subroutine.checkpoint_revert(checkpoint);
                (exit_reason, ret, machine.gas, machine.return_value())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call_inner<SPEC: Spec>(
        &mut self,
        code_address: H160,
        transfer: Option<Transfer>,
        input: Bytes,
        gas_limit: u64,
        context: CallContext,
    ) -> (ExitReason, Gas, Bytes) {
        let mut gas = Gas::new(gas_limit);
        // Load account and get code.
        let (code, _) = self.code(code_address);
        // Create subroutine checkpoint
        let checkpoint = self.subroutine.create_checkpoint();
        self.load_account(context.address);
        // check depth of calls
        // it seems strange but +1 is how geth works, in logs you can see 1025 depth even if 1024 is limit.
        // TODO check +1.
        if self.subroutine.depth() > SPEC::CALL_STACK_LIMIT + 1 {
            return (ExitRevert::CallTooDeep.into(), gas, Bytes::new());
        }
        // transfer value from caller to called account;
        if let Some(transfer) = transfer {
            match self.subroutine.transfer(
                transfer.source,
                transfer.target,
                transfer.value,
                self.db,
            ) {
                Err(e) => {
                    self.subroutine.checkpoint_revert(checkpoint);
                    return (e.into(), gas, Bytes::new());
                }
                Ok((source_is_cold, target_is_cold)) => {
                    if source_is_cold {
                        self.inspector.load_account(&transfer.source);
                    }
                    if target_is_cold {
                        self.inspector.load_account(&transfer.target);
                    }
                }
            }
        }
        // call precompiles
        if let Some(precompile) = self.precompiles.get_fun(&code_address) {
            match (precompile)(input.as_ref(), gas_limit, &context, SPEC::IS_STATIC_CALL) {
                Ok(PrecompileOutput { output, cost, logs }) => {
                    if gas.record_cost(cost) {
                        logs.into_iter()
                            .for_each(|l| self.log(l.address, l.topics, l.data));
                        self.subroutine.checkpoint_commit();
                        (ExitSucceed::Returned.into(), gas, Bytes::from(output))
                    } else {
                        self.subroutine.checkpoint_revert(checkpoint);
                        (ExitError::OutOfGas.into(), gas, Bytes::new())
                    }
                }
                Err(e) => {
                    self.subroutine.checkpoint_revert(checkpoint); //TODO check if we are discarding or reverting
                    (ExitReason::Error(e), gas, Bytes::new())
                }
            }
        } else {
            // create machine and execute subcall
            let contract = Contract::new_with_context(input, code, &context);
            let mut machine = Machine::new::<SPEC>(contract, gas_limit, self.subroutine.depth());
            let exit_reason = machine.run::<Self, SPEC>(self);
            if matches!(exit_reason, ExitReason::Succeed(_)) {
                self.subroutine.checkpoint_commit();
            } else {
                self.subroutine.checkpoint_revert(checkpoint);
            }

            (exit_reason, machine.gas, machine.return_value())
        }
    }
}

impl<'a, DB: Database> Handler for EVM<'a, DB> {
    fn env(&self) -> &GlobalEnv {
        &self.global_env
    }

    fn inspect(&mut self) -> &mut dyn Inspector {
        self.inspector.as_mut()
    }

    fn block_hash(&mut self, number: U256) -> H256 {
        self.db.block_hash(number)
    }

    fn load_account(&mut self, address: H160) -> (bool, bool) {
        let (is_cold, exists) = self.subroutine.load_account_exist(address, self.db);
        if is_cold {
            self.inspector.load_account(&address);
        }
        (is_cold, exists)
    }

    fn balance(&mut self, address: H160) -> (U256, bool) {
        let is_cold = self.inner_load_account(address);
        let balance = self.subroutine.account(address).info.balance;
        (balance, is_cold)
    }

    fn code(&mut self, address: H160) -> (Bytes, bool) {
        let (acc, is_cold) = self.subroutine.load_code(address, self.db);
        if is_cold {
            self.inspector.load_account(&address);
        }
        (acc.info.code.clone().unwrap_or_default(), is_cold)
    }

    /// Get code hash of address.
    fn code_hash(&mut self, address: H160) -> (H256, bool) {
        let (acc, is_cold) = self.subroutine.load_code(address, self.db);
        if is_cold {
            self.inspector.load_account(&address);
        }
        if acc.is_empty() {
            return (H256::zero(), is_cold);
        }

        (acc.info.code_hash.unwrap(), is_cold)
    }

    fn sload(&mut self, address: H160, index: H256) -> (H256, bool) {
        // account is allways hot. reference on that statement https://eips.ethereum.org/EIPS/eip-2929 see `Note 2:`
        self.subroutine.sload(address, index, self.db)
    }

    fn sstore(&mut self, address: H160, index: H256, value: H256) -> (H256, H256, H256, bool) {
        self.subroutine.sstore(address, index, value, self.db)
    }

    fn log(&mut self, address: H160, topics: Vec<H256>, data: Bytes) {
        let log = Log {
            address,
            topics,
            data,
        };
        self.subroutine.log(log);
    }

    fn selfdestruct(&mut self, address: H160, target: H160) -> SelfDestructResult {
        let res = self.subroutine.selfdestruct(address, target, self.db);
        if res.is_cold {
            self.inspector.load_account(&target);
        }
        res
    }

    fn create<SPEC: Spec>(
        &mut self,
        caller: H160,
        scheme: CreateScheme,
        value: U256,
        init_code: Bytes,
        gas: u64,
    ) -> (ExitReason, Option<H160>, Gas, Bytes) {
        self.create_inner::<SPEC>(caller, scheme, value, init_code, gas)
    }

    fn call<SPEC: Spec>(
        &mut self,
        code_address: H160,
        transfer: Option<Transfer>,
        input: Bytes,
        gas: u64,
        context: CallContext,
    ) -> (ExitReason, Gas, Bytes) {
        self.call_inner::<SPEC>(code_address, transfer, input, gas, context)
    }
}

#[derive(Default)]
pub struct SelfDestructResult {
    pub had_value: bool,
    pub exists: bool,
    pub is_cold: bool,
    pub previously_destroyed: bool,
}
/// EVM context handler.
pub trait Handler {
    /// Get global const context of evm execution
    fn env(&self) -> &GlobalEnv;

    fn inspect(&mut self) -> &mut dyn Inspector;

    /// load account. Returns (is_cold,is_new_account)
    fn load_account(&mut self, address: H160) -> (bool, bool);
    /// Get environmental block hash.
    fn block_hash(&mut self, number: U256) -> H256;
    /// Get balance of address.
    fn balance(&mut self, address: H160) -> (U256, bool);
    /// Get code of address.
    fn code(&mut self, address: H160) -> (Bytes, bool);
    /// Get code hash of address.
    fn code_hash(&mut self, address: H160) -> (H256, bool);
    /// Get storage value of address at index.
    fn sload(&mut self, address: H160, index: H256) -> (H256, bool);
    /// Set storage value of address at index. Return if slot is cold/hot access.
    fn sstore(&mut self, address: H160, index: H256, value: H256) -> (H256, H256, H256, bool);
    /// Create a log owned by address with given topics and data.
    fn log(&mut self, address: H160, topics: Vec<H256>, data: Bytes);
    /// Mark an address to be deleted, with funds transferred to target.
    fn selfdestruct(&mut self, address: H160, target: H160) -> SelfDestructResult;
    /// Invoke a create operation.
    fn create<SPEC: Spec>(
        &mut self,
        caller: H160,
        scheme: CreateScheme,
        value: U256,
        init_code: Bytes,
        gas: u64,
    ) -> (ExitReason, Option<H160>, Gas, Bytes);

    /// Invoke a call operation.
    fn call<SPEC: Spec>(
        &mut self,
        code_address: H160,
        transfer: Option<Transfer>,
        input: Bytes,
        gas: u64,
        context: CallContext,
    ) -> (ExitReason, Gas, Bytes);
}
