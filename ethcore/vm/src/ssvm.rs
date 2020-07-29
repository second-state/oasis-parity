use crate::{
	ActionParams, ActionValue, CallType, ContractCreateResult, CreateContractAddress, Ext, GasLeft,
	MessageCallResult, Result, ReturnData, Vm,
};

use ethereum_types::{Address, H256, U256};
use evmc_client::{host::HostContext as HostInterface, load, types as evmc_types};
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::fs::File;
use std::io::Read;
use std::ptr;

pub struct RuntimeContext {
	pub coinbase: Address,
	pub origin: Address,
	pub gas_price: U256,
}

struct HostContext<'a> {
	context: RuntimeContext,
	ext: &'a mut dyn Ext,
}

impl HostInterface for HostContext<'_> {
	fn account_exists(&mut self, addr: &evmc_types::Address) -> bool {
		println!("Host: account_exists");
		self.ext.exists(&Address::from_slice(addr)).unwrap_or(false)
	}
	fn get_storage(
		&mut self,
		_addr: &evmc_types::Address,
		key: &evmc_types::Bytes32,
	) -> evmc_types::Bytes32 {
		println!("Host: get_storage");
		self.ext
			.storage_at(&H256::from_slice(key))
			.unwrap_or(H256::zero())
			.into()
	}
	fn set_storage(
		&mut self,
		_addr: &evmc_types::Address,
		key: &evmc_types::Bytes32,
		value: &evmc_types::Bytes32,
	) -> evmc_types::StorageStatus {
		println!("Host: set_storage");
		let ret: evmc_types::StorageStatus;
		let orig_v = self
			.ext
			.storage_at(&H256::from_slice(key))
			.unwrap_or(H256::zero());
		let new_v = H256::from_slice(value);
		if orig_v.is_zero() {
			ret = evmc_types::StorageStatus::EVMC_STORAGE_ADDED;
		} else if orig_v == new_v {
			ret = evmc_types::StorageStatus::EVMC_STORAGE_UNCHANGED;
		} else {
			ret = evmc_types::StorageStatus::EVMC_STORAGE_MODIFIED;
		}
		if ret != evmc_types::StorageStatus::EVMC_STORAGE_UNCHANGED {
			self.ext.set_storage(H256::from_slice(key), new_v);
		}
		return ret;
	}
	fn get_balance(&mut self, addr: &evmc_types::Address) -> evmc_types::Bytes32 {
		println!("Host: get_balance");
		self.ext
			.balance(&Address::from_slice(addr))
			.unwrap_or(U256::zero())
			.into()
	}
	fn get_code_size(&mut self, addr: &evmc_types::Address) -> usize {
		println!("Host: get_code_size");
		self.ext.extcodesize(&Address::from_slice(addr)).unwrap()
	}
	fn get_code_hash(&mut self, _addr: &evmc_types::Address) -> evmc_types::Bytes32 {
		println!("Host: get_code_hash");
		unimplemented!()
	}
	fn copy_code(
		&mut self,
		addr: &evmc_types::Address,
		offset: &usize,
		buffer_data: &*mut u8,
		buffer_size: &usize,
	) -> usize {
		println!("Host: copy_code");
		let code = self.ext.extcode(&Address::from_slice(addr)).unwrap();
		let remain_size = code.len() - offset;
		let buffer_size = buffer_size.to_owned();
		unsafe {
			let src = code.as_ptr().offset(offset.to_owned().try_into().unwrap());
			ptr::copy(src, buffer_data.to_owned(), buffer_size);
		}
		let ret = if remain_size >= buffer_size {
			buffer_size
		} else {
			remain_size
		};
		return ret;
	}
	fn selfdestruct(&mut self, _addr: &evmc_types::Address, beneficiary: &evmc_types::Address) {
		println!("Host: selfdestruct");
		self.ext.suicide(&Address::from_slice(beneficiary));
	}
	fn get_tx_context(
		&mut self,
	) -> (
		evmc_types::Bytes32,
		evmc_types::Address,
		evmc_types::Address,
		i64,
		i64,
		i64,
		evmc_types::Bytes32,
	) {
		println!("Host: get_tx_context");
		let info = self.ext.env_info();
		return (
			self.context.gas_price.into(),
			self.context.origin.into(),
			self.context.coinbase.into(),
			info.number as i64,
			info.timestamp as i64,
			info.gas_limit.as_u64() as i64,
			info.difficulty.into(),
		);
	}
	fn get_block_hash(&mut self, number: i64) -> evmc_types::Bytes32 {
		println!("Host: get_block_hash");
		self.ext.blockhash(&U256::from(number)).into()
	}
	fn emit_log(
		&mut self,
		_addr: &evmc_types::Address,
		topics: &Vec<evmc_types::Bytes32>,
		data: &[u8],
	) {
		println!("Host: emit_log");
		let ts: Vec<H256> = topics
			.into_iter()
			.map(|topic| H256::from_slice(topic))
			.collect();
		self.ext.log(ts, data);
	}
	fn call(
		&mut self,
		kind: evmc_types::CallKind,
		destination: &evmc_types::Address,
		sender: &evmc_types::Address,
		value: &evmc_types::Bytes32,
		input: &[u8],
		gas: i64,
		_depth: i32,
		is_static: bool,
		salt: &evmc_types::Bytes32,
	) -> (Vec<u8>, i64, evmc_types::Address, evmc_types::StatusCode) {
		println!("Host: call");

		fn convert_calltype(kind: evmc_types::CallKind, _is_static: bool) -> CallType {
			if _is_static {
				return CallType::StaticCall;
			} else if kind == evmc_types::CallKind::EVMC_CALL {
				return CallType::Call;
			} else if kind == evmc_types::CallKind::EVMC_CALLCODE {
				return CallType::CallCode;
			} else if kind == evmc_types::CallKind::EVMC_DELEGATECALL {
				return CallType::DelegateCall;
			} else {
				return CallType::None;
			}
		}

		if kind == evmc_types::CallKind::EVMC_CREATE || kind == evmc_types::CallKind::EVMC_CREATE2 {
			let contract_code = &mut [];
			let result = self.ext.create(
				&U256::from(gas),
				&U256::from(value),
				contract_code,
				if kind == evmc_types::CallKind::EVMC_CREATE {
					CreateContractAddress::FromSenderAndNonce
				} else {
					CreateContractAddress::FromSenderSaltAndCodeHash(H256::from_slice(salt))
				},
			);
			match result {
				ContractCreateResult::Created(address, gas_left) => {
					return (
						contract_code.to_vec(),
						gas_left.as_u64() as i64,
						address.into(),
						evmc_types::StatusCode::EVMC_SUCCESS,
					);
				}
				ContractCreateResult::Failed => {
					return (
						vec![],
						gas,
						[0u8; evmc_types::ADDRESS_LENGTH],
						evmc_types::StatusCode::EVMC_FAILURE,
					);
				}
				ContractCreateResult::Reverted(gas_left, return_data) => {
					return (
						return_data.to_vec(),
						gas_left.as_u64() as i64,
						[0u8; evmc_types::ADDRESS_LENGTH],
						evmc_types::StatusCode::EVMC_REVERT,
					);
				}
			}
		} else {
			let result = self.ext.call(
				&U256::from(gas),
				&Address::from_slice(sender),
				&Address::from_slice(destination),
				Some(U256::from(value)),
				input,
				&Address::from_slice(destination),
				&mut [],
				convert_calltype(kind, is_static),
			);

			match result {
				MessageCallResult::Success(gas_left, return_data) => {
					return (
						return_data.to_vec(),
						gas_left.as_u64() as i64,
						[0u8; evmc_types::ADDRESS_LENGTH],
						evmc_types::StatusCode::EVMC_SUCCESS,
					);
				}
				MessageCallResult::Failed => {
					return (
						vec![],
						gas,
						[0u8; evmc_types::ADDRESS_LENGTH],
						evmc_types::StatusCode::EVMC_FAILURE,
					);
				}
				MessageCallResult::Reverted(gas_left, return_data) => {
					return (
						return_data.to_vec(),
						gas_left.as_u64() as i64,
						[0u8; evmc_types::ADDRESS_LENGTH],
						evmc_types::StatusCode::EVMC_REVERT,
					);
				}
			}
		}
	}
}

pub struct Ssvm;

impl Ssvm {
	pub fn new() -> Self {
		Ssvm {}
	}
}

impl Vm for Ssvm {
	fn prepare(&mut self, params: &ActionParams, ext: &mut Ext) -> Result<()> {
		Ok(())
	}
	fn exec(&mut self, params: ActionParams, ext: &mut Ext) -> Result<GasLeft> {
		let mock_result = Ok(GasLeft::NeedsReturn {
			gas_left: U256::one(),
			data: ReturnData::empty(),
			apply_state: true,
		});
		if cfg!(target_env = "sgx") {
			return mock_result;
		}

		let file_path = "/ssvm/fib.wasm";
		let lib_path = "/ssvm/libssvm-evmc.so";
		match read_a_file(file_path) {
			Ok(code) => {
				let (_vm, _result) = load(lib_path);
				println!("result {:?}", _result);
				println!("Instantiate: {:?}", (_vm.get_name(), _vm.get_version()));

				let runtime_context = RuntimeContext {
					coinbase: params.sender,
					origin: params.origin,
					gas_price: params.gas_price,
				};
				let mut host_context = HostContext {
					context: runtime_context,
					ext: ext,
				};
				let (output, gas_left, status_code) = _vm.execute(
					&mut host_context,
					evmc_types::Revision::EVMC_BYZANTIUM,
					evmc_types::CallKind::EVMC_CALL,
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
				_vm.destroy();
			}
			Err(e) => println!("Error load wasm file: {:?}, {:?}", file_path, e),
		}

		return mock_result;
	}
}

fn read_a_file(path: &str) -> std::io::Result<Vec<u8>> {
	let mut file = File::open(path)?;
	let mut data = Vec::new();
	file.read_to_end(&mut data)?;
	return Ok(data);
}
