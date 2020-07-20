// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.	See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use bytes::Bytes;
use evm::{Factory as EvmFactory, VMType};
use std::{cell::RefCell, rc::Rc};
use vm::{ActionParams, ConfidentialCtx, OasisVm, Schedule, Vm};

use wasm::WasmInterpreter;

use evmc_client::{host::HostContext as HostInterface, load, types::*};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;

const WASM_MAGIC_NUMBER: &'static [u8; 4] = b"\0asm";

/// XXX
struct HostContext {
	storage: BTreeMap<Bytes32, Bytes32>,
}

impl HostContext {
	fn new() -> HostContext {
		HostContext {
			storage: BTreeMap::new(),
		}
	}
}

impl HostInterface for HostContext {
	fn account_exists(&mut self, _addr: &Address) -> bool {
		println!("Host: account_exists");
		return true;
	}
	fn get_storage(&mut self, _addr: &Address, key: &Bytes32) -> Bytes32 {
		println!("Host: get_storage");
		let value = self.storage.get(key);
		let ret: Bytes32;
		match value {
			Some(value) => ret = value.to_owned(),
			None => ret = [0u8; BYTES32_LENGTH],
		}
		println!("{:?} -> {:?}", hex::encode(key), hex::encode(ret));
		return ret;
	}
	fn set_storage(&mut self, _addr: &Address, key: &Bytes32, value: &Bytes32) -> StorageStatus {
		println!("Host: set_storage");
		println!("{:?} -> {:?}", hex::encode(key), hex::encode(value));
		self.storage.insert(key.to_owned(), value.to_owned());
		return StorageStatus::EVMC_STORAGE_MODIFIED;
	}
	fn get_balance(&mut self, _addr: &Address) -> Bytes32 {
		println!("Host: get_balance");
		return [0u8; BYTES32_LENGTH];
	}
	fn get_code_size(&mut self, _addr: &Address) -> usize {
		println!("Host: get_code_size");
		return 0;
	}
	fn get_code_hash(&mut self, _addr: &Address) -> Bytes32 {
		println!("Host: get_code_hash");
		return [0u8; BYTES32_LENGTH];
	}
	fn copy_code(
		&mut self,
		_addr: &Address,
		_offset: &usize,
		_buffer_data: &*mut u8,
		_buffer_size: &usize,
	) -> usize {
		println!("Host: copy_code");
		return 0;
	}
	fn selfdestruct(&mut self, _addr: &Address, _beneficiary: &Address) {
		println!("Host: selfdestruct");
	}
	fn get_tx_context(&mut self) -> (Bytes32, Address, Address, i64, i64, i64, Bytes32) {
		println!("Host: get_tx_context");
		return (
			[0u8; BYTES32_LENGTH],
			[0u8; ADDRESS_LENGTH],
			[0u8; ADDRESS_LENGTH],
			0,
			0,
			0,
			[0u8; BYTES32_LENGTH],
		);
	}
	fn get_block_hash(&mut self, _number: i64) -> Bytes32 {
		println!("Host: get_block_hash");
		return [0u8; BYTES32_LENGTH];
	}
	fn emit_log(&mut self, _addr: &Address, _topics: &Vec<Bytes32>, _data: &[u8]) {
		println!("Host: emit_log");
	}
	fn call(
		&mut self,
		_kind: CallKind,
		_destination: &Address,
		_sender: &Address,
		_value: &Bytes32,
		_input: &[u8],
		_gas: i64,
		_depth: i32,
		_is_static: bool,
	) -> (Vec<u8>, i64, Address, StatusCode) {
		println!("Host: call");
		return (
			vec![0u8; BYTES32_LENGTH],
			_gas,
			[0u8; ADDRESS_LENGTH],
			StatusCode::EVMC_SUCCESS,
		);
	}
}

impl Drop for HostContext {
	fn drop(&mut self) {
		println!("Dump storage:");
		for (key, value) in &self.storage {
			println!("{:?} -> {:?}", hex::encode(key), hex::encode(value));
		}
	}
}

fn read_a_file(path: &str) -> std::io::Result<Vec<u8>> {
	let mut file = File::open(path)?;
	let mut data = Vec::new();
	file.read_to_end(&mut data)?;
	return Ok(data);
}

#[cfg(not(target_env = "sgx"))]
fn exec(lib_path: &str, file_path: &str) {
	let (_vm, _result) = load(lib_path);
	println!("result {:?}", _result);
	println!("Instantiate: {:?}", (_vm.get_name(), _vm.get_version()));
	match read_a_file(file_path) {
		Ok(code) => {
			let host_context = HostContext::new();
			let (output, gas_left, status_code) = _vm.execute(
				Box::new(host_context),
				Revision::EVMC_BYZANTIUM,
				CallKind::EVMC_CALL,
				false,
				123,
				50000000,
				&[32u8; 20],
				&[128u8; 20],
				&[0u8; 0],
				&[0u8; 32],
				&code[..],
				&[0u8; 32],
			);
			println!("Output:  {:?}", hex::encode(output));
			println!("GasLeft: {:?}", gas_left);
			println!("Status:  {:?}", status_code);
		}
		Err(e) => println!("Error load wasm file: {:?}, {:?}", file_path, e),
	}
	_vm.destroy();
}

#[cfg(target_env = "sgx")]
fn exec(lib_path: &str, file_path: &str) {
	println!("SSVM not support in SGX")
}

/// Virtual machine factory
#[derive(Default, Clone)]
pub struct VmFactory {
	evm: EvmFactory,
}

impl VmFactory {
	pub fn create(
		&self,
		ctx: Option<Rc<RefCell<Box<ConfidentialCtx>>>>,
		params: &ActionParams,
		schedule: &Schedule,
	) -> Box<Vm> {
		println!("VmFactory create");
		println!("Schedule {:?}", schedule.wasm.is_some());
		println!("code {:?}", params.code);
		let vm = {
			if schedule.wasm.is_some()
				&& params.code.as_ref().map_or(false, |code| {
					code.len() > 4 && &code[0..4] == WASM_MAGIC_NUMBER
				}) {
				/// XXX
				exec("/ssvm/libssvm-evmc.so", "/ssvm/fib.wasm");
				Box::new(WasmInterpreter)
			} else {
				self.evm.create(&params.gas)
			}
		};
		Box::new(OasisVm::new(ctx, vm))
	}

	pub fn new(evm: VMType, cache_size: usize) -> Self {
		VmFactory {
			evm: EvmFactory::new(evm, cache_size),
		}
	}
}

impl From<EvmFactory> for VmFactory {
	fn from(evm: EvmFactory) -> Self {
		VmFactory { evm: evm }
	}
}

/// Collection of factories.
#[derive(Default, Clone)]
pub struct Factories {
	/// factory for evm.
	pub vm: VmFactory,
}
