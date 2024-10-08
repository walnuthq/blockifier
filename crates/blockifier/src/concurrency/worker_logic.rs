use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use cairo_felt::Felt252;
use num_traits::Bounded;
use starknet_api::core::{ClassHash, ContractAddress};
use starknet_api::hash::StarkFelt;
use starknet_api::transaction::Fee;

use super::versioned_state::VersionedStateProxy;
use crate::concurrency::fee_utils::fill_sequencer_balance_reads;
use crate::concurrency::scheduler::{Scheduler, Task};
use crate::concurrency::utils::lock_mutex_in_array;
use crate::concurrency::versioned_state::ThreadSafeVersionedState;
use crate::concurrency::TxIndex;
use crate::context::BlockContext;
use crate::execution::execution_utils::{felt_to_stark_felt, stark_felt_to_felt};
use crate::fee::fee_utils::get_sequencer_balance_keys;
use crate::state::cached_state::{CachedState, ContractClassMapping, StateMaps};
use crate::state::state_api::{StateReader, StateResult};
use crate::transaction::objects::{TransactionExecutionInfo, TransactionExecutionResult};
use crate::transaction::transaction_execution::Transaction;
use crate::transaction::transactions::ExecutableTransaction;

const EXECUTION_OUTPUTS_UNWRAP_ERROR: &str = "Execution task outputs should not be None.";

#[cfg(test)]
#[path = "worker_logic_test.rs"]
pub mod test;

#[derive(Debug)]
pub struct ExecutionTaskOutput {
    pub reads: StateMaps,
    pub writes: StateMaps,
    pub visited_pcs: HashMap<ClassHash, HashSet<usize>>,
    pub result: TransactionExecutionResult<TransactionExecutionInfo>,
}

pub struct WorkerExecutor<'a, S: StateReader> {
    pub scheduler: Scheduler,
    pub state: ThreadSafeVersionedState<S>,
    pub chunk: &'a [Transaction],
    pub execution_outputs: Box<[Mutex<Option<ExecutionTaskOutput>>]>,
    pub block_context: BlockContext,
}
impl<'a, S: StateReader> WorkerExecutor<'a, S> {
    pub fn new(
        state: ThreadSafeVersionedState<S>,
        chunk: &'a [Transaction],
        block_context: BlockContext,
    ) -> Self {
        let scheduler = Scheduler::new(chunk.len());
        let execution_outputs =
            std::iter::repeat_with(|| Mutex::new(None)).take(chunk.len()).collect();

        WorkerExecutor { scheduler, state, chunk, execution_outputs, block_context }
    }

    pub fn run(&self) {
        let mut task = Task::NoTask;
        loop {
            task = match task {
                Task::ExecutionTask(tx_index) => {
                    self.execute(tx_index);
                    Task::NoTask
                }
                Task::ValidationTask(tx_index) => self.validate(tx_index),
                Task::NoTask => self.scheduler.next_task(),
                Task::Done => break,
            };
        }
    }

    fn execute(&self, tx_index: TxIndex) {
        self.execute_tx(tx_index);
        self.scheduler.finish_execution(tx_index)
    }

    fn execute_tx(&self, tx_index: TxIndex) {
        let tx_versioned_state = self.state.pin_version(tx_index);
        let tx = &self.chunk[tx_index];
        // TODO(Noa, 15/05/2024): remove the redundant cached state.
        let mut tx_state = CachedState::new(tx_versioned_state);
        let mut transactional_state = CachedState::create_transactional(&mut tx_state);
        let validate = true;
        let charge_fee = true;

        let execution_result =
            tx.execute_raw(&mut transactional_state, &self.block_context, charge_fee, validate);

        if execution_result.is_ok() {
            let class_hash_to_class = transactional_state.class_hash_to_class.borrow();
            // TODO(Noa, 15/05/2024): use `tx_versioned_state` when we add support to transactional
            // versioned state.
            self.state
                .pin_version(tx_index)
                .apply_writes(&transactional_state.cache.borrow().writes, &class_hash_to_class);
        }

        // Write the transaction execution outputs.
        let tx_reads_writes = transactional_state.cache.take();
        // In case of a failed transaction, we don't record its writes and visited pcs.
        let (writes, visited_pcs) = match execution_result {
            Ok(_) => (tx_reads_writes.writes, transactional_state.visited_pcs),
            Err(_) => (StateMaps::default(), HashMap::default()),
        };
        let mut execution_output = lock_mutex_in_array(&self.execution_outputs, tx_index);
        *execution_output = Some(ExecutionTaskOutput {
            reads: tx_reads_writes.initial_reads,
            writes,
            visited_pcs,
            result: execution_result,
        });
    }

    fn validate(&self, _tx_index: TxIndex) -> Task {
        todo!();
    }

    /// Commits a transaction.
    /// First we validate the read set:
    /// If the validation failed, we delete the transaction writes and (re-)execute it.
    /// Else (validation succeeded) no need to re-execute.
    /// Now that the transaction execution is final:
    /// If execution succeeded, we ask the bouncer if there is room
    /// for the transaction in the block.
    /// If there is room: we fix the call info, update the sequencer balance and
    /// commit the transaction. Otherwise (execution failed, no room), we don't commit.
    pub fn commit_tx(&self, tx_index: TxIndex) -> StateResult<bool> {
        let execution_output = lock_mutex_in_array(&self.execution_outputs, tx_index);

        let tx = &self.chunk[tx_index];
        let tx_versioned_state = self.state.pin_version(tx_index);

        let read_set = &execution_output.as_ref().expect(EXECUTION_OUTPUTS_UNWRAP_ERROR).reads;
        let validate_reads = tx_versioned_state.validate_reads(read_set);
        drop(execution_output);

        // First, re-validate the transaction.
        if !validate_reads {
            // Revalidate failed: re-execute the transaction, and commit.
            // TODO(Meshi, 01/06/2024): Delete the transaction writes.
            self.execute_tx(tx_index);
            let execution_output = lock_mutex_in_array(&self.execution_outputs, tx_index);
            let read_set = &execution_output.as_ref().expect(EXECUTION_OUTPUTS_UNWRAP_ERROR).reads;
            // Another validation after the re-execution for sanity check.
            assert!(tx_versioned_state.validate_reads(read_set));
        }

        // Execution is final.
        let mut execution_output = lock_mutex_in_array(&self.execution_outputs, tx_index);
        let result_tx_info =
            &mut execution_output.as_mut().expect(EXECUTION_OUTPUTS_UNWRAP_ERROR).result;

        let tx_context = Arc::new(self.block_context.to_tx_context(tx));
        // Fix the sequencer balance.
        // There is no need to fix the balance when the sequencer transfers fee to itself, since we
        // use the sequential fee transfer in this case.
        if let Ok(tx_info) = result_tx_info.as_mut() {
            // TODO(Meshi, 01/06/2024): check if this is also needed in the bouncer.
            if tx_context.tx_info.sender_address()
                != self.block_context.block_info.sequencer_address
            {
                // Update the sequencer balance in the storage.
                let mut next_tx_versioned_state = self.state.pin_version(tx_index + 1);
                let (sequencer_balance_value_low, sequencer_balance_value_high) =
                    next_tx_versioned_state.get_fee_token_balance(
                        tx_context.block_context.block_info.sequencer_address,
                        tx_context.fee_token_address(),
                    )?;
                if let Some(fee_transfer_call_info) = tx_info.fee_transfer_call_info.as_mut() {
                    // Fix the transfer call info.
                    fill_sequencer_balance_reads(
                        fee_transfer_call_info,
                        sequencer_balance_value_low,
                        sequencer_balance_value_high,
                    );
                }
                add_fee_to_sequencer_balance(
                    tx_context.fee_token_address(),
                    &tx_versioned_state,
                    tx_info.actual_fee,
                    &self.block_context,
                    sequencer_balance_value_low,
                    sequencer_balance_value_high,
                );
            }
        }

        Ok(true)
    }
}

fn add_fee_to_sequencer_balance(
    fee_token_address: ContractAddress,
    tx_versioned_state: &VersionedStateProxy<impl StateReader>,
    actual_fee: Fee,
    block_context: &BlockContext,
    sequencer_balance_value_low: StarkFelt,
    sequencer_balance_value_high: StarkFelt,
) {
    let (sequencer_balance_key_low, sequencer_balance_key_high) =
        get_sequencer_balance_keys(block_context);
    let felt_fee = &Felt252::from(actual_fee.0);
    let new_value_low = stark_felt_to_felt(sequencer_balance_value_low) + felt_fee;
    let overflow =
        stark_felt_to_felt(sequencer_balance_value_low) > Felt252::max_value() - felt_fee;
    let new_value_high = if overflow {
        stark_felt_to_felt(sequencer_balance_value_high) + Felt252::from(1_u8)
    } else {
        stark_felt_to_felt(sequencer_balance_value_high)
    };

    let writes = StateMaps {
        storage: HashMap::from([
            ((fee_token_address, sequencer_balance_key_low), felt_to_stark_felt(&new_value_low)),
            ((fee_token_address, sequencer_balance_key_high), felt_to_stark_felt(&new_value_high)),
        ]),
        ..StateMaps::default()
    };
    tx_versioned_state.apply_writes(&writes, &ContractClassMapping::default());
}
