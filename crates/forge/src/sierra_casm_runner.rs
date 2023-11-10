use blockifier::execution::syscalls::hint_processor::SyscallHintProcessor;
use std::collections::HashMap;

use crate::test_execution_syscall_handler::TestExecutionSyscallHandler;
use cairo_felt::Felt252;
use cairo_lang_casm::instructions::Instruction;
use cairo_lang_casm::{casm, casm_extend};
use cairo_lang_runner::casm_run::RunFunctionContext;
pub use cairo_lang_runner::casm_run::{CairoHintProcessor, StarknetState};
use cairo_lang_runner::{token_gas_cost, Arg, RunResult, RunResultValue, RunnerError};
use cairo_lang_sierra::extensions::bitwise::BitwiseType;
use cairo_lang_sierra::extensions::core::{CoreLibfunc, CoreType};
use cairo_lang_sierra::extensions::ec::EcOpType;
use cairo_lang_sierra::extensions::enm::EnumType;
use cairo_lang_sierra::extensions::gas::{CostTokenType, GasBuiltinType};
use cairo_lang_sierra::extensions::pedersen::PedersenType;
use cairo_lang_sierra::extensions::poseidon::PoseidonType;
use cairo_lang_sierra::extensions::range_check::RangeCheckType;
use cairo_lang_sierra::extensions::segment_arena::SegmentArenaType;
use cairo_lang_sierra::extensions::starknet::syscalls::SystemType;
use cairo_lang_sierra::extensions::{ConcreteType, NamedType};
use cairo_lang_sierra::program::{Function, GenericArg};
use cairo_lang_sierra::program_registry::ProgramRegistry;
use cairo_lang_sierra_ap_change::calc_ap_changes;
use cairo_lang_sierra_gas::gas_info::GasInfo;
use cairo_lang_sierra_to_casm::compiler::CairoProgram;
use cairo_lang_sierra_to_casm::metadata::{
    calc_metadata, Metadata, MetadataComputationConfig, MetadataError,
};
use cairo_lang_sierra_type_size::{get_type_size_map, TypeSizeMap};
use cairo_lang_starknet::contract::ContractInfo;
use cairo_lang_utils::casts::IntoOrPanic;
use cairo_lang_utils::extract_matches;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_vm::serde::deserialize_program::{BuiltinName, HintParams, ReferenceManager};
use cairo_vm::types::program::Program;
use cairo_vm::types::relocatable::MaybeRelocatable;
use cairo_vm::vm::errors::cairo_run_errors::CairoRunError;
use cairo_vm::vm::errors::vm_errors::VirtualMachineError;
use cairo_vm::vm::runners::cairo_runner::CairoRunner;
use cairo_vm::vm::vm_core::VirtualMachine;
use num_traits::ToPrimitive;

// almost entirely copied from cairo_lang_runner
// fields are `pub` now
pub struct SierraCasmRunner {
    /// The sierra program.
    pub sierra_program: cairo_lang_sierra::program::Program,
    /// Metadata for the Sierra program.
    pub metadata: Metadata,
    /// Program registry for the Sierra program.
    pub sierra_program_registry: ProgramRegistry<CoreType, CoreLibfunc>,
    /// Program registry for the Sierra program.
    pub type_sizes: TypeSizeMap,
    /// The casm program matching the Sierra code.
    pub casm_program: CairoProgram,
    #[allow(dead_code)]
    // Mapping from class_hash to contract info.
    pub starknet_contracts_info: OrderedHashMap<Felt252, ContractInfo>,
}
impl SierraCasmRunner {
    pub fn new(
        sierra_program: cairo_lang_sierra::program::Program,
        metadata_config: Option<MetadataComputationConfig>,
        starknet_contracts_info: OrderedHashMap<Felt252, ContractInfo>,
    ) -> Result<Self, RunnerError> {
        let gas_usage_check = metadata_config.is_some();
        let metadata = create_metadata(&sierra_program, metadata_config)?;
        let sierra_program_registry =
            ProgramRegistry::<CoreType, CoreLibfunc>::new(&sierra_program)?;
        let type_sizes = get_type_size_map(&sierra_program, &sierra_program_registry).unwrap();
        let casm_program = cairo_lang_sierra_to_casm::compiler::compile(
            &sierra_program,
            &metadata,
            gas_usage_check,
        )?;

        // Find all contracts.
        Ok(Self {
            sierra_program,
            metadata,
            sierra_program_registry,
            type_sizes,
            casm_program,
            starknet_contracts_info,
        })
    }

    /// Runs the vm starting from a function with custom hint processor. Function may have
    /// implicits, but no other ref params. The cost of the function is deducted from
    /// `available_gas` before the execution begins.
    ///
    /// Allows injecting Cairo `VirtualMachine`
    #[allow(clippy::cast_sign_loss)]
    pub fn run_function_with_vm<'a, Instructions>(
        &self,
        func: &Function,
        vm: &mut VirtualMachine,
        hint_processor: &mut TestExecutionSyscallHandler,
        hints_dict: HashMap<usize, Vec<HintParams>>,
        instructions: Instructions,
        builtins: Vec<BuiltinName>,
    ) -> Result<RunResult, RunnerError>
    where
        Instructions: Iterator<Item = &'a Instruction> + Clone,
    {
        // changed from casm_run::fun_function to our implementation
        let (cells, ap) = run_function(
            vm,
            instructions,
            builtins,
            |context| {
                let vm = context.vm;
                // Create the builtin cost segment, with dummy values.
                let builtin_cost_segment = vm.add_memory_segment();
                for token_type in CostTokenType::iter_precost() {
                    vm.insert_value(
                        (builtin_cost_segment + (token_type.offset_in_builtin_costs() as usize))
                            .unwrap(),
                        Felt252::from(token_gas_cost(*token_type)),
                    )
                    .map_err(|e| Box::new(e.into()))?;
                }
                // Put a pointer to the builtin cost segment at the end of the program (after the
                // additional `ret` statement).
                vm.insert_value(
                    (vm.get_pc() + context.data_len).unwrap(),
                    builtin_cost_segment,
                )
                .map_err(|e| Box::new(e.into()))?;
                Ok(())
            },
            hint_processor,
            hints_dict,
        )?;
        let mut results_data = self.get_results_data(func, &cells, ap);
        // Handling implicits.
        let mut gas_counter = None;
        results_data.retain_mut(|(ty, values)| {
            let info = self.get_info(ty);
            let generic_ty = &info.long_id.generic_id;
            if *generic_ty == GasBuiltinType::ID {
                gas_counter = Some(values.remove(0));
                assert!(values.is_empty());
                false
            } else {
                *generic_ty != RangeCheckType::ID
                    && *generic_ty != BitwiseType::ID
                    && *generic_ty != EcOpType::ID
                    && *generic_ty != PedersenType::ID
                    && *generic_ty != PoseidonType::ID
                    && *generic_ty != SystemType::ID
                    && *generic_ty != SegmentArenaType::ID
            }
        });
        assert!(results_data.len() <= 1);
        let value = if results_data.is_empty() {
            // No result type - no panic.
            RunResultValue::Success(vec![])
        } else {
            let [(ty, values)] = <[_; 1]>::try_from(results_data).ok().unwrap();
            self.handle_main_return_value(&ty, values, &cells)
        };

        Ok(RunResult {
            gas_counter,
            memory: cells,
            value,
        })
    }

    /// Handling the main return value to create a `RunResultValue`.
    #[allow(clippy::cast_sign_loss)]
    fn handle_main_return_value(
        &self,
        ty: &cairo_lang_sierra::ids::ConcreteTypeId,
        values: Vec<Felt252>,
        cells: &[Option<Felt252>],
    ) -> RunResultValue {
        let info = self.get_info(ty);
        let long_id = &info.long_id;
        if long_id.generic_id == EnumType::ID
            && matches!(&long_id.generic_args[0], GenericArg::UserType(ut)
                if ut.debug_name.as_ref().unwrap().starts_with("core::panics::PanicResult::"))
        {
            // The function includes a panic wrapper.
            if values[0] == Felt252::from(0) {
                // The run resulted successfully, returning the inner value.
                let inner_ty = extract_matches!(&long_id.generic_args[1], GenericArg::Type);
                let inner_ty_size = self.type_sizes[inner_ty] as usize;
                let skip_size = values.len() - inner_ty_size;
                RunResultValue::Success(values.into_iter().skip(skip_size).collect())
            } else {
                // The run resulted in a panic, returning the error data.
                let err_data_start = values[values.len() - 2].to_usize().unwrap();
                let err_data_end = values[values.len() - 1].to_usize().unwrap();
                RunResultValue::Panic(
                    cells[err_data_start..err_data_end]
                        .iter()
                        .cloned()
                        .map(std::option::Option::unwrap)
                        .collect(),
                )
            }
        } else {
            // No panic wrap - so always successful.
            RunResultValue::Success(values)
        }
    }

    /// Returns the final values and type of all `func`s returning variables.
    #[allow(clippy::cast_sign_loss)]
    fn get_results_data(
        &self,
        func: &Function,
        cells: &[Option<Felt252>],
        mut ap: usize,
    ) -> Vec<(cairo_lang_sierra::ids::ConcreteTypeId, Vec<Felt252>)> {
        let mut results_data = vec![];
        for ty in func.signature.ret_types.iter().rev() {
            let size = self.type_sizes[ty] as usize;
            let values: Vec<Felt252> = ((ap - size)..ap)
                .map(|index| cells[index].clone().unwrap())
                .collect();
            ap -= size;
            results_data.push((ty.clone(), values));
        }
        results_data
    }

    /// Finds first function ending with `name_suffix`.
    pub fn find_function(&self, name_suffix: &str) -> Result<&Function, RunnerError> {
        self.sierra_program
            .funcs
            .iter()
            .find(|f| {
                if let Some(name) = &f.id.debug_name {
                    name.ends_with(name_suffix)
                } else {
                    false
                }
            })
            .ok_or_else(|| RunnerError::MissingFunction {
                suffix: name_suffix.to_owned(),
            })
    }

    fn get_info(
        &self,
        ty: &cairo_lang_sierra::ids::ConcreteTypeId,
    ) -> &cairo_lang_sierra::extensions::types::TypeInfo {
        self.sierra_program_registry.get_type(ty).unwrap().info()
    }

    /// Returns the instructions to add to the beginning of the code to successfully call the main
    /// function, as well as the builtins required to execute the program.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cast_sign_loss)]
    pub fn create_entry_code(
        &self,
        func: &Function,
        args: &[Arg],
        initial_gas: usize,
    ) -> Result<(Vec<Instruction>, Vec<BuiltinName>), RunnerError> {
        let mut arg_iter = args.iter().peekable();
        let mut expected_arguments_size = 0;
        let mut ctx = casm! {};
        // The builtins in the formatting expected by the runner.
        let builtins = vec![
            BuiltinName::pedersen,
            BuiltinName::range_check,
            BuiltinName::bitwise,
            BuiltinName::ec_op,
            BuiltinName::poseidon,
        ];
        // The offset [fp - i] for each of this builtins in this configuration.
        let builtin_offset: HashMap<cairo_lang_sierra::ids::GenericTypeId, i16> = HashMap::from([
            (PedersenType::ID, 7),
            (RangeCheckType::ID, 6),
            (BitwiseType::ID, 5),
            (EcOpType::ID, 4),
            (PoseidonType::ID, 3),
        ]);
        // Load all vecs to memory.
        let mut vecs = vec![];
        let mut ap_offset: i16 = 0;
        for arg in args {
            let Arg::Array(values) = arg else { continue };
            vecs.push(ap_offset);
            casm_extend! {ctx,
                %{ memory[ap + 0] = segments.add() %}
                ap += 1;
            }
            for (i, v) in values.iter().enumerate() {
                let arr_at = i16::try_from(i + 1).unwrap();
                casm_extend! {ctx,
                    [ap + 0] = (v.to_bigint());
                    [ap + 0] = [[ap - arr_at] + i16::try_from(i).unwrap()], ap++;
                };
            }
            ap_offset += i16::try_from(1 + values.len()).unwrap();
        }
        let after_vecs_offset = ap_offset;
        if func
            .signature
            .param_types
            .iter()
            .any(|ty| self.get_info(ty).long_id.generic_id == SegmentArenaType::ID)
        {
            casm_extend! {ctx,
                // SegmentArena segment.
                %{ memory[ap + 0] = segments.add() %}
                // Infos segment.
                %{ memory[ap + 1] = segments.add() %}
                ap += 2;
                [ap + 0] = 0, ap++;
                // Write Infos segment, n_constructed (0), and n_destructed (0) to the segment.
                [ap - 2] = [[ap - 3]];
                [ap - 1] = [[ap - 3] + 1];
                [ap - 1] = [[ap - 3] + 2];
            }
            ap_offset += 3;
        }
        for ty in &func.signature.param_types {
            let info = self.get_info(ty);
            let ty_size = self.type_sizes[ty];
            let generic_ty = &info.long_id.generic_id;
            if let Some(offset) = builtin_offset.get(generic_ty) {
                casm_extend! {ctx,
                    [ap + 0] = [fp - offset], ap++;
                }
            } else if generic_ty == &SystemType::ID {
                casm_extend! {ctx,
                    %{ memory[ap + 0] = segments.add() %}
                    ap += 1;
                }
            } else if generic_ty == &GasBuiltinType::ID {
                casm_extend! {ctx,
                    [ap + 0] = initial_gas, ap++;
                }
            } else if generic_ty == &SegmentArenaType::ID {
                let offset = -ap_offset + after_vecs_offset;
                casm_extend! {ctx,
                    [ap + 0] = [ap + offset] + 3, ap++;
                }
            } else if let Some(Arg::Array(_)) = arg_iter.peek() {
                let values = extract_matches!(arg_iter.next().unwrap(), Arg::Array);
                let offset = -ap_offset + vecs.pop().unwrap();
                expected_arguments_size += 1;
                casm_extend! {ctx,
                    [ap + 0] = [ap + (offset)], ap++;
                    [ap + 0] = [ap - 1] + (values.len()), ap++;
                }
            } else {
                let arg_size = ty_size;
                expected_arguments_size += arg_size as usize;
                for _ in 0..arg_size {
                    if let Some(value) = arg_iter.next() {
                        let value = extract_matches!(value, Arg::Value);
                        casm_extend! {ctx,
                            [ap + 0] = (value.to_bigint()), ap++;
                        }
                    }
                }
            };
            ap_offset += ty_size;
        }
        if expected_arguments_size != args.len() {
            return Err(RunnerError::ArgumentsSizeMismatch {
                expected: expected_arguments_size,
                actual: args.len(),
            });
        }
        let before_final_call = ctx.current_code_offset;
        let final_call_size = 3;
        let offset = final_call_size
            + self.casm_program.debug_info.sierra_statement_info[func.entry_point.0].code_offset;
        casm_extend! {ctx,
            call rel offset;
            ret;
        }
        assert_eq!(before_final_call + final_call_size, ctx.current_code_offset);
        Ok((ctx.instructions, builtins))
    }

    /// Returns the initial value for the gas counter.
    /// If `available_gas` is None returns 0.
    pub fn get_initial_available_gas(
        &self,
        func: &Function,
        available_gas: Option<usize>,
    ) -> Result<usize, RunnerError> {
        let Some(available_gas) = available_gas else {
            return Ok(0);
        };

        // In case we don't have any costs - it means no gas equations were solved (and we are in
        // the case of no gas checking enabled) - so the gas builtin is irrelevant, and we
        // can return any value.
        let Some(required_gas) = self.initial_required_gas(func) else {
            return Ok(0);
        };

        available_gas
            .checked_sub(required_gas)
            .ok_or(RunnerError::NotEnoughGasToCall)
    }

    pub fn initial_required_gas(&self, func: &Function) -> Option<usize> {
        if self.metadata.gas_info.function_costs.is_empty() {
            return None;
        }
        Some(
            self.metadata.gas_info.function_costs[func.id.clone()]
                .iter()
                .map(|(token_type, val)| val.into_or_panic::<usize>() * token_gas_cost(*token_type))
                .sum(),
        )
    }

    /// Creates a list of instructions that will be appended to the program's bytecode.
    pub fn create_code_footer() -> Vec<Instruction> {
        casm! {
            // Add a `ret` instruction used in libfuncs that retrieve the current value of the `fp`
            // and `pc` registers.
            ret;
        }
        .instructions
    }

    pub fn get_casm_program(&self) -> &CairoProgram {
        &self.casm_program
    }
}

fn create_metadata(
    sierra_program: &cairo_lang_sierra::program::Program,
    metadata_config: Option<MetadataComputationConfig>,
) -> Result<Metadata, RunnerError> {
    if let Some(metadata_config) = metadata_config {
        calc_metadata(sierra_program, metadata_config, false).map_err(|err| match err {
            MetadataError::ApChangeError(err) => RunnerError::ApChangeError(err),
            MetadataError::CostError(_) => RunnerError::FailedGasCalculation,
        })
    } else {
        Ok(Metadata {
            ap_change_info: calc_ap_changes(sierra_program, |_, _| 0)?,
            gas_info: GasInfo {
                variable_values: Default::default(),
                function_costs: Default::default(),
            },
        })
    }
}

// casm_run::run_function
pub fn run_function<'a, 'b: 'a, Instructions>(
    vm: &mut VirtualMachine,
    instructions: Instructions,
    builtins: Vec<BuiltinName>,
    additional_initialization: fn(
        context: RunFunctionContext<'_>,
    ) -> Result<(), Box<CairoRunError>>,
    hint_processor: &mut TestExecutionSyscallHandler,
    hints_dict: HashMap<usize, Vec<HintParams>>,
) -> Result<(Vec<Option<Felt252>>, usize), Box<CairoRunError>>
where
    Instructions: Iterator<Item = &'a Instruction> + Clone,
{
    let data: Vec<MaybeRelocatable> = instructions
        .flat_map(|inst| inst.assemble().encode())
        .map(Felt252::from)
        .map(MaybeRelocatable::from)
        .collect();

    let data_len = data.len();
    let program = Program::new(
        builtins,
        data,
        Some(0),
        hints_dict,
        ReferenceManager {
            references: Vec::new(),
        },
        HashMap::new(),
        vec![],
        None,
    )
    .map_err(CairoRunError::from)?;
    let mut runner = CairoRunner::new(&program, "starknet", false)
        .map_err(CairoRunError::from)
        .map_err(Box::new)?;

    let end = runner.initialize(vm).map_err(CairoRunError::from)?;

    additional_initialization(RunFunctionContext { vm, data_len })?;

    runner
        .run_until_pc(end, vm, hint_processor)
        .map_err(CairoRunError::from)?;
    runner
        .end_run(true, false, vm, hint_processor)
        .map_err(CairoRunError::from)?;
    runner.relocate(vm, true).map_err(CairoRunError::from)?;

    // changed region
    finalize(vm, &runner, &mut hint_processor.child.child.child, 0, 2);
    // end region

    Ok((
        runner.relocated_memory,
        vm.get_relocated_trace().unwrap().last().unwrap().ap,
    ))
}

// similar to `finalize_execution` from blockifier
fn finalize(
    vm: &mut VirtualMachine,
    runner: &CairoRunner,
    syscall_handler: &mut SyscallHintProcessor<'_>,
    n_total_args: usize,
    program_extra_data_length: usize,
) {
    let program_start_ptr = runner
        .program_base
        .expect("The `program_base` field should be initialized after running the entry point.");
    let program_end_ptr = (program_start_ptr + runner.get_program().data_len()).unwrap();
    vm.mark_address_range_as_accessed(program_end_ptr, program_extra_data_length)
        .unwrap();

    let initial_fp = runner
        .get_initial_fp()
        .expect("The `initial_fp` field should be initialized after running the entry point.");
    // When execution starts the stack holds the EP arguments + [ret_fp, ret_pc].
    let args_ptr = (initial_fp - (n_total_args + 2)).unwrap();
    vm.mark_address_range_as_accessed(args_ptr, n_total_args)
        .unwrap();
    syscall_handler
        .read_only_segments
        .mark_as_accessed(vm)
        .unwrap();

    let vm_resources_without_inner_calls = runner
        .get_execution_resources(vm)
        .map_err(VirtualMachineError::TracerError)
        .unwrap()
        .filter_unused_builtins();
    syscall_handler.resources.vm_resources += &vm_resources_without_inner_calls;
}
