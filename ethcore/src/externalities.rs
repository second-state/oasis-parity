// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Transaction Execution environment.
use bytes::{Bytes, BytesRef};
use ethereum_types::{Address, H256, U256};
use evm::FinalizationResult;
use executive::*;
use machine::EthereumMachine as Machine;
use state::{Backend as StateBackend, CleanupMode, State, Substate};
use std::cmp;
use std::sync::Arc;
use trace::{Tracer, VMTracer};
use trace_ext::ExtTracer;
#[cfg(test)]
use trace_ext::NoopExtTracer;
use transaction::UNSIGNED_SENDER;
use vm::{
	self, ActionParams, ActionValue, CallType, ContractCreateResult, CreateContractAddress,
	EnvInfo, Ext, MessageCallResult, OasisContract, OasisContractHeader, ReturnData, Schedule,
};

/// Policy for handling output data on `RETURN` opcode.
pub enum OutputPolicy<'a, 'b> {
	/// Return reference to fixed sized output.
	/// Used for message calls.
	Return(BytesRef<'a>, Option<&'b mut Bytes>),
	/// Init new contract as soon as `RETURN` is called.
	InitContract(Option<&'b mut Bytes>),
}

/// Transaction properties that externalities need to know about.
#[derive(Default)]
pub struct OriginInfo {
	address: Address,
	origin: Address,
	origin_nonce: U256,
	gas_price: U256,
	value: U256,
}

impl OriginInfo {
	/// Populates origin info from action params.
	pub fn from(params: &ActionParams) -> Self {
		OriginInfo {
			address: params.address.clone(),
			origin: params.origin.clone(),
			origin_nonce: params.origin_nonce,
			gas_price: params.gas_price,
			value: match params.value {
				ActionValue::Transfer(val) | ActionValue::Apparent(val) => val,
			},
		}
	}
}

/// Implementation of evm Externalities.
pub struct Externalities<'a, T: 'a, V: 'a, X: 'a, B: 'a>
where
	T: Tracer,
	V: VMTracer,
	X: ExtTracer,
	B: StateBackend,
{
	state: &'a mut State<B>,
	env_info: &'a EnvInfo,
	machine: &'a Machine,
	depth: usize,
	origin_info: OriginInfo,
	substate: &'a mut Substate,
	schedule: Schedule,
	output: OutputPolicy<'a, 'a>,
	tracer: &'a mut T,
	vm_tracer: &'a mut V,
	ext_tracer: &'a mut X,
	static_flag: bool,
}

impl<'a, T: 'a, V: 'a, X: 'a, B: 'a> Externalities<'a, T, V, X, B>
where
	T: Tracer,
	V: VMTracer,
	X: ExtTracer,
	B: StateBackend,
{
	/// Basic `Externalities` constructor.
	pub fn new(
		state: &'a mut State<B>,
		env_info: &'a EnvInfo,
		machine: &'a Machine,
		depth: usize,
		origin_info: OriginInfo,
		substate: &'a mut Substate,
		output: OutputPolicy<'a, 'a>,
		tracer: &'a mut T,
		vm_tracer: &'a mut V,
		ext_tracer: &'a mut X,
		static_flag: bool,
	) -> Self {
		Externalities {
			state: state,
			env_info: env_info,
			machine: machine,
			depth: depth,
			origin_info: origin_info,
			substate: substate,
			schedule: machine.schedule(env_info.number),
			output: output,
			tracer: tracer,
			vm_tracer: vm_tracer,
			ext_tracer: ext_tracer,
			static_flag: static_flag,
		}
	}
}

impl<'a, T: 'a, V: 'a, X: 'a, B: 'a> Ext for Externalities<'a, T, V, X, B>
where
	T: Tracer,
	V: VMTracer,
	X: ExtTracer,
	B: StateBackend,
{
	fn storage_at(&self, key: &H256) -> vm::Result<H256> {
		self.ext_tracer.trace_storage_at(key);
		self.state
			.storage_at(&self.origin_info.address, key)
			.map_err(Into::into)
	}

	fn set_storage(&mut self, key: H256, value: H256) -> vm::Result<()> {
		if self.static_flag {
			Err(vm::Error::MutableCallInStaticContext)
		} else {
			self.ext_tracer.trace_set_storage(&key);
			self.state
				.set_storage(&self.origin_info.address, key, value)
				.map_err(Into::into)
		}
	}

	fn storage_bytes_at(&self, key: &H256) -> vm::Result<Vec<u8>> {
		self.ext_tracer.trace_storage_at(key);
		self.state
			.storage_bytes_at(&self.origin_info.address, key)
			.map_err(Into::into)
	}

	fn storage_bytes_len(&self, key: &H256) -> vm::Result<u64> {
		self.state
			.storage_bytes_at(&self.origin_info.address, key)
			.map(|bytes| bytes.len() as u64)
			.map_err(Into::into)
	}

	fn set_storage_bytes(&mut self, key: H256, value: Vec<u8>) -> vm::Result<()> {
		if self.static_flag {
			Err(vm::Error::MutableCallInStaticContext)
		} else {
			self.ext_tracer.trace_set_storage(&key);
			self.state
				.set_storage_bytes(&self.origin_info.address, key, value)
				.map_err(Into::into)
		}
	}

	fn storage_expiry(&self, addr: &Address) -> vm::Result<u64> {
		self.state.storage_expiry(addr).map_err(Into::into)
	}

	fn seconds_until_expiry(&self) -> vm::Result<u64> {
		let current_timestamp = self.env_info.timestamp;
		let expiry_timestamp = self.storage_expiry(&self.origin_info.address)?;
		if current_timestamp > expiry_timestamp {
			return Err(vm::Error::ContractExpired);
		}
		Ok(expiry_timestamp - current_timestamp)
	}

	fn is_static(&self) -> bool {
		return self.static_flag;
	}

	fn is_create(&self) -> bool {
		match self.output {
			OutputPolicy::InitContract(_) => true,
			_ => false,
		}
	}

	fn exists(&self, address: &Address) -> vm::Result<bool> {
		self.ext_tracer.trace_exists(address);
		self.state.exists(address).map_err(Into::into)
	}

	fn exists_and_not_null(&self, address: &Address) -> vm::Result<bool> {
		self.ext_tracer.trace_exists_and_not_null(address);
		self.state.exists_and_not_null(address).map_err(Into::into)
	}

	fn origin_balance(&self) -> vm::Result<U256> {
		self.ext_tracer.trace_balance(&self.origin_info.address);
		self.balance(&self.origin_info.address).map_err(Into::into)
	}

	fn origin_nonce(&self) -> U256 {
		self.origin_info.origin_nonce
	}

	fn balance(&self, address: &Address) -> vm::Result<U256> {
		self.ext_tracer.trace_balance(address);
		self.state.balance(address).map_err(Into::into)
	}

	fn blockhash(&mut self, number: &U256) -> H256 {
		if *number < U256::from(self.env_info.number)
			&& number.low_u64() >= cmp::max(256, self.env_info.number) - 256
		{
			let index = self.env_info.number - number.low_u64() - 1;
			self.env_info.last_hashes[index as usize].clone()
		} else {
			H256::zero()
		}
	}

	fn create(
		&mut self,
		gas: &U256,
		value: &U256,
		code: &[u8],
		address_scheme: CreateContractAddress,
	) -> ContractCreateResult {
		let code = {
			// The following block sets the `confidential` field of the new
			// contract's header if it isn't already when the creator is a
			// confidential contract. This is done to prevent confidential
			// data from accidentially leaking into a non-confidential context.
			match &self.state.confidential_ctx {
				Some(ctx) if ctx.borrow().activated() => {
					let mut new_header: Option<OasisContractHeader> = None;
					let mut headerless_code = code.to_vec();
					match OasisContract::from_code(code) {
						Ok(Some(oc)) if !oc.confidential => {
							new_header = Some(OasisContractHeader::V1 {
								confidential: Some(true),
								expiry: oc.expiry,
							});
							headerless_code = oc.code.to_vec();
						}
						Ok(Some(_)) => {} // already has a valid header
						Ok(None) => {
							new_header = Some(OasisContractHeader::V1 {
								confidential: Some(true),
								expiry: None,
							});
						}
						Err(_) => return ContractCreateResult::Failed,
					}
					match new_header {
						Some(h) => h.to_vec().into_iter().chain(headerless_code).collect(),
						None => headerless_code,
					}
				}
				_ => code.to_vec(),
			}
		};

		// create new contract address
		let (address, code_hash) = match self.state.nonce(&self.origin_info.address) {
			Ok(nonce) => contract_address(address_scheme, &self.origin_info.address, &nonce, &code),
			Err(e) => {
				debug!(target: "ext", "Database corruption encountered: {:?}", e);
				return ContractCreateResult::Failed;
			}
		};

		// Extract contract deployment header, if present.
		let oasis_contract = match OasisContract::from_code(&code) {
			Ok(contract) => contract,
			Err(_) => return ContractCreateResult::Failed,
		};

		// prepare the params
		let params = ActionParams {
			code_address: address.clone(),
			address: address.clone(),
			sender: self.origin_info.address.clone(),
			origin: self.origin_info.origin.clone(),
			origin_nonce: self.origin_info.origin_nonce,
			gas: *gas,
			gas_price: self.origin_info.gas_price,
			value: ActionValue::Transfer(*value),
			// Code stripped of contract header, if present.
			code: Some(
				oasis_contract
					.as_ref()
					.map_or(Arc::new(code.to_vec()), |c| c.code.clone()),
			),
			code_hash: code_hash,
			data: None,
			call_type: CallType::None,
			params_type: vm::ParamsType::Embedded,
			oasis_contract: oasis_contract,
			aad: None,
		};

		if !self.static_flag {
			if params.sender != UNSIGNED_SENDER {
				if let Err(e) = self.state.inc_nonce(&self.origin_info.address) {
					debug!(target: "ext", "Database corruption encountered: {:?}", e);
					return ContractCreateResult::Failed;
				}
			}
		}
		let mut ex = Executive::from_parent(
			self.state,
			self.env_info,
			self.machine,
			self.depth,
			self.static_flag,
		);

		// TODO: handle internal error separately
		match ex.create(
			params,
			self.substate,
			&mut None,
			self.tracer,
			self.vm_tracer,
			self.ext_tracer,
		) {
			Ok(FinalizationResult {
				gas_left,
				apply_state: true,
				..
			}) => {
				self.substate.contracts_created.push(address.clone());
				ContractCreateResult::Created(address, gas_left)
			}
			Ok(FinalizationResult {
				gas_left,
				apply_state: false,
				return_data,
			}) => ContractCreateResult::Reverted(gas_left, return_data),
			_ => ContractCreateResult::Failed,
		}
	}

	fn call(
		&mut self,
		gas: &U256,
		sender_address: &Address,
		receive_address: &Address,
		value: Option<U256>,
		data: &[u8],
		code_address: &Address,
		output: &mut [u8],
		call_type: CallType,
	) -> MessageCallResult {
		trace!(target: "externalities", "call");

		let code_res = self
			.state
			.code(code_address)
			.and_then(|code| self.state.code_hash(code_address).map(|hash| (code, hash)));

		let (code, code_hash) = match code_res {
			Ok((code, hash)) => (code, hash),
			Err(_) => return MessageCallResult::Failed,
		};

		// Extract contract deployment header, if present.
		let oasis_contract = if let Some(ref code) = code {
			match OasisContract::from_code(code) {
				Ok(contract) => contract,
				Err(_) => return MessageCallResult::Failed,
			}
		} else {
			None
		};

		let mut params = ActionParams {
			sender: sender_address.clone(),
			address: receive_address.clone(),
			value: ActionValue::Apparent(self.origin_info.value),
			code_address: code_address.clone(),
			origin: self.origin_info.origin.clone(),
			origin_nonce: self.origin_info.origin_nonce,
			gas: *gas,
			gas_price: self.origin_info.gas_price,
			// Code stripped of contract header, if present.
			code: oasis_contract
				.as_ref()
				.map_or(code, |c| Some(c.code.clone())),
			code_hash: Some(code_hash),
			data: Some(data.to_vec()),
			call_type: call_type,
			params_type: vm::ParamsType::Separate,
			oasis_contract: oasis_contract,
			aad: None, // will be populated by ConfidentialVM if in c10l context
		};

		if let Some(value) = value {
			params.value = ActionValue::Transfer(value);
		}

		let mut ex = Executive::from_parent(
			self.state,
			self.env_info,
			self.machine,
			self.depth,
			self.static_flag,
		);

		let mut subexttracer = self.ext_tracer.subtracer(&params.address);
		match ex.call(
			params,
			self.substate,
			BytesRef::Fixed(output),
			self.tracer,
			self.vm_tracer,
			&mut subexttracer,
		) {
			Ok(FinalizationResult {
				gas_left,
				return_data,
				apply_state: true,
			}) => MessageCallResult::Success(gas_left, return_data),
			Ok(FinalizationResult {
				gas_left,
				return_data,
				apply_state: false,
			}) => MessageCallResult::Reverted(gas_left, return_data),
			_ => MessageCallResult::Failed,
		}
	}

	fn extcode(&self, address: &Address) -> vm::Result<Arc<Bytes>> {
		Ok(self
			.state
			.code(address)?
			.unwrap_or_else(|| Arc::new(vec![])))
	}

	fn extcodesize(&self, address: &Address) -> vm::Result<usize> {
		Ok(self.state.code_size(address)?.unwrap_or(0))
	}

	fn ret(mut self, gas: &U256, data: &ReturnData, apply_state: bool) -> vm::Result<U256>
	where
		Self: Sized,
	{
		let handle_copy = |to: &mut Option<&mut Bytes>| {
			to.as_mut().map(|b| **b = data.to_vec());
		};
		match self.output {
			OutputPolicy::Return(BytesRef::Fixed(ref mut slice), ref mut copy) => {
				handle_copy(copy);

				let len = cmp::min(slice.len(), data.len());
				(&mut slice[..len]).copy_from_slice(&data[..len]);
				Ok(*gas)
			}
			OutputPolicy::Return(BytesRef::Flexible(ref mut vec), ref mut copy) => {
				handle_copy(copy);

				vec.clear();
				vec.extend_from_slice(&*data);
				Ok(*gas)
			}
			OutputPolicy::InitContract(ref mut copy) if apply_state => {
				let return_cost =
					U256::from(data.len()) * U256::from(self.schedule.create_data_gas);
				if return_cost > *gas || data.len() > self.schedule.create_data_limit {
					return match self.schedule.exceptional_failed_code_deposit {
						true => Err(vm::Error::OutOfGas),
						false => Ok(*gas),
					};
				}
				handle_copy(copy);
				self.state
					.init_code(&self.origin_info.address, data.to_vec())?;
				Ok(*gas - return_cost)
			}
			OutputPolicy::InitContract(_) => Ok(*gas),
		}
	}

	fn log(&mut self, topics: Vec<H256>, data: &[u8]) -> vm::Result<()> {
		use log_entry::LogEntry;

		if self.static_flag {
			return Err(vm::Error::MutableCallInStaticContext);
		}

		let address = self.origin_info.address.clone();

		self.substate.logs.push(LogEntry {
			address: address,
			topics: topics,
			data: data.to_vec(),
		});

		Ok(())
	}

	fn suicide(&mut self, refund_address: &Address) -> vm::Result<()> {
		if self.static_flag {
			return Err(vm::Error::MutableCallInStaticContext);
		}

		let address = self.origin_info.address.clone();
		let balance = self.balance(&address)?;
		if &address == refund_address {
			// TODO [todr] To be consistent with CPP client we set balance to 0 in that case.
			self.state
				.sub_balance(&address, &balance, &mut CleanupMode::NoEmpty)?;
		} else {
			trace!(target: "ext", "Suiciding {} -> {} (xfer: {})", address, refund_address, balance);
			self.state.transfer_balance(
				&address,
				refund_address,
				&balance,
				self.substate.to_cleanup_mode(&self.schedule),
			)?;
		}

		self.tracer
			.trace_suicide(address, balance, refund_address.clone());
		self.substate.suicides.insert(address);

		Ok(())
	}

	fn schedule(&self) -> &Schedule {
		&self.schedule
	}

	fn env_info(&self) -> &EnvInfo {
		self.env_info
	}

	fn depth(&self) -> usize {
		self.depth
	}

	/// Updates gas refund for an SSTORE clear
	fn inc_sstore_clears(&mut self, bytes_len: u64) -> vm::Result<()> {
		// gas refund prorated based on time until expiry
		let duration_secs = self.seconds_until_expiry()?;

		let refund = self
			.schedule
			.prorated_sstore_refund_gas(duration_secs, bytes_len);
		self.substate.sstore_clears_refund = self.substate.sstore_clears_refund + refund;

		Ok(())
	}

	fn trace_next_instruction(&mut self, pc: usize, instruction: u8, current_gas: U256) -> bool {
		self.vm_tracer
			.trace_next_instruction(pc, instruction, current_gas)
	}

	fn trace_prepare_execute(&mut self, pc: usize, instruction: u8, gas_cost: U256) {
		self.vm_tracer
			.trace_prepare_execute(pc, instruction, gas_cost)
	}

	fn trace_executed(
		&mut self,
		gas_used: U256,
		stack_push: &[U256],
		mem_diff: Option<(usize, &[u8])>,
		store_diff: Option<(U256, U256)>,
	) {
		self.vm_tracer
			.trace_executed(gas_used, stack_push, mem_diff, store_diff)
	}

	fn is_confidential_contract(&self, contract: &Address) -> vm::Result<bool> {
		self.state
			.is_confidential_contract(contract)
			.map_err(|err| vm::Error::Confidential(err))
	}

	fn as_kvstore(&self) -> &dyn blockchain_traits::KVStore {
		self
	}

	fn as_kvstore_mut(&mut self) -> &mut dyn blockchain_traits::KVStoreMut {
		self
	}
}

/// The Parity trie uses H256 (32-byte) keys. Keys used by WASI services
/// will look like file names. Trie performance is optimized when keys with
/// similar access patterns share a prefix. This function aims to maximize
/// performance by preserving the original (hopefully prefixed) paths but
/// safely defaults to hashing long paths.
fn slice_to_key(sl: &[u8]) -> H256 {
	let mut hash = [0u8; 32];
	if sl.len() > hash.len() {
		keccak_hash::keccak_256(sl, &mut hash);
	} else {
		hash[..sl.len()].copy_from_slice(sl);
	}
	H256::from(hash)
}

impl<'a, T: 'a, V: 'a, X: 'a, B: 'a> blockchain_traits::KVStore for Externalities<'a, T, V, X, B>
where
	T: Tracer,
	V: VMTracer,
	X: ExtTracer,
	B: StateBackend,
{
	fn contains(&self, key: &[u8]) -> bool {
		let key = self.state.to_storage_key(&slice_to_key(key));
		self.state
			._storage_at(&self.origin_info.address, &key)
			.as_ref()
			.map(Option::is_some)
			.unwrap_or_default()
	}

	fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
		self.storage_bytes_at(&slice_to_key(key)).ok()
	}
}

impl<'a, T: 'a, V: 'a, X: 'a, B: 'a> blockchain_traits::KVStoreMut for Externalities<'a, T, V, X, B>
where
	T: Tracer,
	V: VMTracer,
	X: ExtTracer,
	B: StateBackend,
{
	fn set(&mut self, key: &[u8], value: &[u8]) {
		self.set_storage_bytes(slice_to_key(key), value.to_vec())
			.ok();
	}

	fn remove(&mut self, key: &[u8]) {
		self.set_storage_bytes(slice_to_key(key), Vec::new()).ok();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ethereum_types::{Address, U256};
	use evm::{CallType, EnvInfo, Ext};
	use state::{State, Substate};
	use test_helpers::{self, get_temp_state};
	use trace::{NoopTracer, NoopVMTracer};

	fn get_test_origin() -> OriginInfo {
		Default::default()
	}

	fn get_test_env_info() -> EnvInfo {
		EnvInfo {
			number: 100,
			author: 0.into(),
			timestamp: 0,
			difficulty: 0.into(),
			last_hashes: Arc::new(vec![]),
			gas_used: 0.into(),
			gas_limit: 0.into(),
		}
	}

	struct TestSetup {
		state: State<::state_db::StateDB>,
		machine: ::machine::EthereumMachine,
		sub_state: Substate,
		env_info: EnvInfo,
	}

	impl Default for TestSetup {
		fn default() -> Self {
			TestSetup::new()
		}
	}

	impl TestSetup {
		fn new() -> Self {
			TestSetup {
				state: get_temp_state(),
				machine: ::spec::Spec::new_test_machine(),
				sub_state: Substate::new(),
				env_info: get_test_env_info(),
			}
		}
	}

	#[test]
	fn can_be_created() {
		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		let ext = Externalities::new(
			state,
			&setup.env_info,
			&setup.machine,
			0,
			get_test_origin(),
			&mut setup.sub_state,
			OutputPolicy::InitContract(None),
			&mut tracer,
			&mut vm_tracer,
			&mut ext_tracer,
			false,
		);

		assert_eq!(ext.env_info().number, 100);
	}

	#[test]
	fn can_return_block_hash_no_env() {
		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		let mut ext = Externalities::new(
			state,
			&setup.env_info,
			&setup.machine,
			0,
			get_test_origin(),
			&mut setup.sub_state,
			OutputPolicy::InitContract(None),
			&mut tracer,
			&mut vm_tracer,
			&mut ext_tracer,
			false,
		);

		let hash = ext.blockhash(
			&"0000000000000000000000000000000000000000000000000000000000120000"
				.parse::<U256>()
				.unwrap(),
		);

		assert_eq!(hash, H256::zero());
	}

	#[test]
	fn can_return_block_hash() {
		let test_hash =
			H256::from("afafafafafafafafafafafbcbcbcbcbcbcbcbcbcbeeeeeeeeeeeeedddddddddd");
		let test_env_number = 0x120001;

		let mut setup = TestSetup::new();
		{
			let env_info = &mut setup.env_info;
			env_info.number = test_env_number;
			let mut last_hashes = (*env_info.last_hashes).clone();
			last_hashes.push(test_hash.clone());
			env_info.last_hashes = Arc::new(last_hashes);
		}
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		let mut ext = Externalities::new(
			state,
			&setup.env_info,
			&setup.machine,
			0,
			get_test_origin(),
			&mut setup.sub_state,
			OutputPolicy::InitContract(None),
			&mut tracer,
			&mut vm_tracer,
			&mut ext_tracer,
			false,
		);

		let hash = ext.blockhash(
			&"0000000000000000000000000000000000000000000000000000000000120000"
				.parse::<U256>()
				.unwrap(),
		);

		assert_eq!(test_hash, hash);
	}

	#[test]
	#[should_panic]
	fn can_call_fail_empty() {
		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		let mut ext = Externalities::new(
			state,
			&setup.env_info,
			&setup.machine,
			0,
			get_test_origin(),
			&mut setup.sub_state,
			OutputPolicy::InitContract(None),
			&mut tracer,
			&mut vm_tracer,
			&mut ext_tracer,
			false,
		);

		let mut output = vec![];

		// this should panic because we have no balance on any account
		ext.call(
			&"0000000000000000000000000000000000000000000000000000000000120000"
				.parse::<U256>()
				.unwrap(),
			&Address::new(),
			&Address::new(),
			Some(
				"0000000000000000000000000000000000000000000000000000000000150000"
					.parse::<U256>()
					.unwrap(),
			),
			&[],
			&Address::new(),
			&mut output,
			CallType::Call,
		);
	}

	#[test]
	fn can_log() {
		let log_data = vec![120u8, 110u8];
		let log_topics = vec![H256::from(
			"af0fa234a6af46afa23faf23bcbc1c1cb4bcb7bcbe7e7e7ee3ee2edddddddddd",
		)];

		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		{
			let mut ext = Externalities::new(
				state,
				&setup.env_info,
				&setup.machine,
				0,
				get_test_origin(),
				&mut setup.sub_state,
				OutputPolicy::InitContract(None),
				&mut tracer,
				&mut vm_tracer,
				&mut ext_tracer,
				false,
			);
			ext.log(log_topics, &log_data).unwrap();
		}

		assert_eq!(setup.sub_state.logs.len(), 1);
	}

	#[test]
	fn can_suicide() {
		let refund_account = &Address::new();

		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		{
			let mut ext = Externalities::new(
				state,
				&setup.env_info,
				&setup.machine,
				0,
				get_test_origin(),
				&mut setup.sub_state,
				OutputPolicy::InitContract(None),
				&mut tracer,
				&mut vm_tracer,
				&mut ext_tracer,
				false,
			);
			ext.suicide(refund_account).unwrap();
		}

		assert_eq!(setup.sub_state.suicides.len(), 1);
	}

	#[test]
	fn can_create() {
		use std::str::FromStr;

		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		let address = {
			let mut ext = Externalities::new(
				state,
				&setup.env_info,
				&setup.machine,
				0,
				get_test_origin(),
				&mut setup.sub_state,
				OutputPolicy::InitContract(None),
				&mut tracer,
				&mut vm_tracer,
				&mut ext_tracer,
				false,
			);
			match ext.create(
				&U256::max_value(),
				&U256::zero(),
				&[],
				CreateContractAddress::FromSenderAndNonce,
			) {
				ContractCreateResult::Created(address, _) => address,
				_ => panic!("Test create failed; expected Created, got Failed/Reverted."),
			}
		};

		assert_eq!(
			address,
			Address::from_str("bd770416a3345f91e4b34576cb804a576fa48eb1").unwrap()
		);
	}

	#[test]
	fn can_create2() {
		use std::str::FromStr;

		let mut setup = TestSetup::new();
		let state = &mut setup.state;
		let mut tracer = NoopTracer;
		let mut vm_tracer = NoopVMTracer;
		let mut ext_tracer = NoopExtTracer;

		let address = {
			let mut ext = Externalities::new(
				state,
				&setup.env_info,
				&setup.machine,
				0,
				get_test_origin(),
				&mut setup.sub_state,
				OutputPolicy::InitContract(None),
				&mut tracer,
				&mut vm_tracer,
				&mut ext_tracer,
				false,
			);
			match ext.create(
				&U256::max_value(),
				&U256::zero(),
				&[],
				CreateContractAddress::FromSenderSaltAndCodeHash(H256::default()),
			) {
				ContractCreateResult::Created(address, _) => address,
				_ => panic!("Test create failed; expected Created, got Failed/Reverted."),
			}
		};

		assert_eq!(
			address,
			Address::from_str("b7c227636666831278bacdb8d7f52933b8698ab9").unwrap()
		);
	}

	#[test]
	fn create_confidential() {
		use rustc_hex::{FromHex as _, ToHex as _};
		use std::{cell::RefCell, rc::Rc, str::FromStr, sync::Arc};
		use vm::ConfidentialCtx as _;

		let create_and_get_code = |code: Vec<u8>| -> Result<Arc<Vec<u8>>, ContractCreateResult> {
			let mut setup = TestSetup::new();

			let state = &mut setup.state;
			let c10l_ctx = test_helpers::MockConfidentialContext::activated();
			state.confidential_ctx = Some(Rc::new(RefCell::new(Box::new(c10l_ctx))));

			let mut tracer = NoopTracer;
			let mut vm_tracer = NoopVMTracer;
			let mut ext_tracer = NoopExtTracer;

			let mut ext = Externalities::new(
				state,
				&setup.env_info,
				&setup.machine,
				0,
				get_test_origin(),
				&mut setup.sub_state,
				OutputPolicy::InitContract(None),
				&mut tracer,
				&mut vm_tracer,
				&mut ext_tracer,
				false,
			);
			let address = {
				match ext.create(
					&U256::max_value(),
					&U256::zero(),
					&code,
					CreateContractAddress::FromSenderSaltAndCodeHash(H256::default()),
				) {
					ContractCreateResult::Created(address, _) => address,
					res => return Err(res),
				}
			};
			Ok(state.code(&address).unwrap().unwrap())
		};

		// Empty contract compiled using Remix IDE.
		let deploycode_hex = "6080604052348015600f57600080fd5b50603f80601d6000396000f3fe6080604052600080fdfea26469706673582212200604656e3d2eb983e78496078d39b74441869c89fe04d88e9d653aadd8ffcebd64736f6c63430006010033";
		let deploycode = deploycode_hex.from_hex().unwrap();

		let expected_code_hex = "6080604052600080fdfea26469706673582212200604656e3d2eb983e78496078d39b74441869c89fe04d88e9d653aadd8ffcebd64736f6c63430006010033";

		let expect_code = |header: Option<OasisContractHeader>| {
			let mut headered_deploycode = header.as_ref().map(|h| h.to_vec()).unwrap_or_default();
			headered_deploycode.extend(&deploycode);
			let contract_code = match create_and_get_code(headered_deploycode) {
				Ok(code) => code,
				Err(_) => panic!("contract create failed"),
			};
			let oc = OasisContract::from_code(&contract_code).unwrap().unwrap();
			assert_eq!(oc.header_version, 1);
			assert_eq!(oc.confidential, true);
			assert_eq!(
				oc.expiry,
				header.and_then(|h| match h {
					OasisContractHeader::V1 { expiry, .. } => expiry,
				})
			);
			assert_eq!(oc.code.to_hex(), expected_code_hex);
		};

		// expect default header to be added to headerless deploycode
		expect_code(None);

		// expect that `confidential` is set to true and expiry is preserved
		expect_code(Some(OasisContractHeader::V1 {
			confidential: None,
			expiry: None,
		}));
		expect_code(Some(OasisContractHeader::V1 {
			confidential: Some(false),
			expiry: Some(1),
		}));
		expect_code(Some(OasisContractHeader::V1 {
			confidential: None,
			expiry: Some(2),
		}));
		expect_code(Some(OasisContractHeader::V1 {
			confidential: Some(true),
			expiry: Some(3),
		}));

		// expect that invalid Oasis header causes failure
		let header = OasisContractHeader::V1 {
			confidential: None,
			expiry: None,
		};
		let mut invalid_headered_deploycode = vm::OASIS_HEADER_PREFIX.to_vec();
		invalid_headered_deploycode.extend(&deploycode);
		assert!(match create_and_get_code(invalid_headered_deploycode) {
			Err(ContractCreateResult::Failed) => true,
			_ => false,
		});
	}
}
