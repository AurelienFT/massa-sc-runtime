use super::{as_abi::*, MassaModule};
use crate::env::{
    assembly_script_abort, assembly_script_date, assembly_script_seed, get_remaining_points,
    set_remaining_points, ASEnv, MassaEnv,
};
use crate::types::Response;
use crate::{GasCosts, Interface};
use anyhow::{bail, Result};
use as_ffi_bindings::{BufferPtr, Read as ASRead, Write as ASWrite};
use wasmer::{imports, Function, FunctionEnv, Imports, Instance, Store, Value};

pub(crate) struct ASModule {
    env: ASEnv,
    bytecode: Vec<u8>,
}

impl MassaModule for ASModule {
    fn init(interface: &dyn Interface, bytecode: &[u8], gas_costs: GasCosts) -> Self {
        Self {
            env: ASEnv::new(interface, gas_costs),
            bytecode: bytecode.to_vec(),
        }
    }

    fn get_bytecode(&self) -> &Vec<u8> {
        &self.bytecode
    }

    fn execution(
        &self,
        instance: &Instance,
        store: &mut Store,
        function: &str,
        param: &[u8],
    ) -> Result<Response> {
        if cfg!(not(feature = "gas_calibration")) {
            // sub initial metering cost
            let metering_initial_cost = self.env.get_gas_costs().launch_cost;
            let remaining_gas = get_remaining_points(&self.env, store)?;
            if metering_initial_cost > remaining_gas {
                bail!("Not enough gas to launch the virtual machine")
            }
            set_remaining_points(&self.env, store, remaining_gas - metering_initial_cost)?;
        }

        // Now can exec
        let wasm_func = instance.exports.get_function(function)?;
        let argc = wasm_func.param_arity(store);
        let res = if argc == 0 && function == crate::settings::MAIN {
            wasm_func.call(store, &[])
        } else if argc == 1 {
            let param_ptr = *BufferPtr::alloc(&param.to_vec(), self.env.get_wasm_env(), store)?;
            wasm_func.call(store, &[Value::I32(param_ptr.offset() as i32)])
        } else {
            bail!("Unexpected number of parameters in the function called")
        };

        match res {
            Ok(value) => {
                if function.eq(crate::settings::MAIN) {
                    let remaining_gas = if cfg!(feature = "gas_calibration") {
                        Ok(0_u64)
                    } else {
                        get_remaining_points(&self.env, store)
                    };

                    return Ok(Response {
                        ret: Vec::new(), // main return empty vec
                        remaining_gas: remaining_gas?,
                    });
                }
                let ret = if let Some(offset) = value.get(0) {
                    if let Some(offset) = offset.i32() {
                        let buffer_ptr = BufferPtr::new(offset as u32);
                        let memory = instance.exports.get_memory("memory")?;
                        buffer_ptr.read(memory, store)?
                    } else {
                        bail!("Execution wasn't in capacity to read the return value")
                    }
                } else {
                    Vec::new()
                };
                let remaining_gas = if cfg!(feature = "gas_calibration") {
                    Ok(0_u64)
                } else {
                    get_remaining_points(&self.env, store)
                };
                Ok(Response {
                    ret,
                    remaining_gas: remaining_gas?,
                })
            }
            Err(error) => bail!(error),
        }
    }

    fn init_with_instance(
        &mut self,
        instance: &Instance,
        store: &mut Store,
        fenv: &mut FunctionEnv<ASEnv>,
    ) -> Result<()> {
        let memory = instance.exports.get_memory("memory")?;

        // Note: only add functions (__new, ...) if these exists in wasm/wat files
        //       so we can still exec some very basic wat files
        let fn_new = instance
            .exports
            .get_typed_function::<(i32, i32), i32>(&store, "__new")
            .ok();
        let fn_pin = instance
            .exports
            .get_typed_function::<i32, i32>(&store, "__pin")
            .ok();
        let fn_unpin = instance
            .exports
            .get_typed_function::<i32, ()>(&store, "__unpin")
            .ok();
        let fn_collect = instance
            .exports
            .get_typed_function::<(), ()>(&store, "__collect")
            .ok();

        fenv.as_mut(store).get_wasm_env_as_mut().init_with(
            Some(memory.clone()),
            fn_new.clone(),
            fn_pin.clone(),
            fn_unpin.clone(),
            fn_collect.clone(),
        );

        // Update self.env as well
        self.env.get_wasm_env_as_mut().init_with(
            Some(memory.clone()),
            fn_new,
            fn_pin,
            fn_unpin,
            fn_collect,
        );

        // metering counters
        if cfg!(not(feature = "gas_calibration")) {
            let g_1 = instance
                .exports
                .get_global("wasmer_metering_remaining_points")?
                .clone();
            fenv.as_mut(store).remaining_points = Some(g_1.clone());
            let g_2 = instance
                .exports
                .get_global("wasmer_metering_points_exhausted")?
                .clone();
            fenv.as_mut(store).exhausted_points = Some(g_2.clone());

            self.env.remaining_points = Some(g_1);
            self.env.exhausted_points = Some(g_2);
        }

        Ok(())
    }

    fn has_function(&self, instance: &Instance, function: &str) -> bool {
        instance.exports.get_function(function).is_ok()
    }

    fn get_gas_costs(&self) -> GasCosts {
        self.env.get_gas_costs()
    }

    fn resolver(&self, store: &mut Store) -> (Imports, FunctionEnv<ASEnv>) {
        let fenv = FunctionEnv::new(store, self.env.clone());

        let imports = imports! {
            "env" => {
                // Needed by wasm generated by AssemblyScript.
                "abort" =>  Function::new_typed_with_env(store, &fenv.clone(), assembly_script_abort),
                "seed" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_seed),
                "Date.now" =>  Function::new_typed_with_env(store, &fenv.clone(), assembly_script_date),
            },
            "massa" => {
                "assembly_script_print" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_print),
                "assembly_script_call" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_call),
                "assembly_script_get_remaining_gas" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_remaining_gas),
                "assembly_script_create_sc" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_create_sc),
                "assembly_script_set_data" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_set_data),
                "assembly_script_set_data_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_set_data_for),
                "assembly_script_get_data" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_data),
                "assembly_script_get_data_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_data_for),
                "assembly_script_delete_data" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_delete_data),
                "assembly_script_delete_data_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_delete_data_for),
                "assembly_script_append_data" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_append_data),
                "assembly_script_append_data_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_append_data_for),
                "assembly_script_has_data" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_has_data),
                "assembly_script_has_data_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_has_data_for),
                "assembly_script_get_owned_addresses" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_owned_addresses),
                "assembly_script_get_call_stack" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_call_stack),
                "assembly_script_generate_event" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_generate_event),
                "assembly_script_transfer_coins" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_transfer_coins),
                "assembly_script_transfer_coins_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_transfer_coins_for),
                "assembly_script_get_balance" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_balance),
                "assembly_script_get_balance_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_balance_for),
                "assembly_script_hash" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_hash),
                "assembly_script_signature_verify" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_signature_verify),
                "assembly_script_address_from_public_key" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_address_from_public_key),
                "assembly_script_unsafe_random" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_unsafe_random),
                "assembly_script_get_call_coins" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_call_coins),
                "assembly_script_get_time" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_time),
                "assembly_script_send_message" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_send_message),
                "assembly_script_get_current_period" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_current_period),
                "assembly_script_get_current_thread" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_current_thread),
                "assembly_script_set_bytecode" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_set_bytecode),
                "assembly_script_set_bytecode_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_set_bytecode_for),
                "assembly_script_get_op_keys" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_op_keys),
                "assembly_script_get_keys" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_keys),
                "assembly_script_get_keys_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_keys_for),
                "assembly_script_has_op_key" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_has_op_key),
                "assembly_script_get_op_data" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_op_data),
                "assembly_script_get_bytecode" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_bytecode),
                "assembly_script_get_bytecode_for" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_get_bytecode_for),
                "assembly_script_local_call" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_local_call),
                "assembly_script_local_execution" => Function::new_typed_with_env(store, &fenv.clone(), assembly_script_local_execution),
                "assembly_caller_has_write_access" => Function::new_typed_with_env(store, &fenv.clone(), assembly_caller_has_write_access),
                "assembly_function_exists" => Function::new_typed_with_env(store, &fenv.clone(), assembly_function_exists),
            },
        };

        (imports, fenv)
    }
}
