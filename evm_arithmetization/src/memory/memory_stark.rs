use core::marker::PhantomData;
use std::borrow::Borrow;

use ethereum_types::U256;
use itertools::Itertools;
use plonky2::field::extension::{Extendable, FieldExtension};
use plonky2::field::packed::PackedField;
use plonky2::field::polynomial::PolynomialValues;
use plonky2::field::types::Field;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::ext_target::ExtensionTarget;
use plonky2::timed;
use plonky2::util::timing::TimingTree;
use plonky2::util::transpose;
use plonky2_maybe_rayon::*;
use starky::constraint_consumer::{ConstraintConsumer, RecursiveConstraintConsumer};
use starky::cross_table_lookup::TableWithColumns;
use starky::evaluation_frame::StarkEvaluationFrame;
use starky::lookup::{Column, Filter, Lookup};
use starky::stark::Stark;

use super::columns::{MemoryColumnsView, MEMORY_COL_MAP};
use super::segments::{Segment, PREINITIALIZED_SEGMENTS_INDICES};
use crate::all_stark::{EvmStarkFrame, Table};
use crate::memory::columns::NUM_COLUMNS;
use crate::memory::VALUE_LIMBS;
use crate::witness::memory::MemoryOpKind::{self, Read};
use crate::witness::memory::{MemoryAddress, MemoryOp};

/// Creates the vector of `Columns` corresponding to:
/// - the memory operation type,
/// - the address in memory of the element being read/written,
/// - the value being read/written,
/// - the timestamp at which the element is read/written.
pub(crate) fn ctl_data<F: Field>() -> Vec<Column<F>> {
    let mut res = Column::singles([
        MEMORY_COL_MAP.is_read,
        MEMORY_COL_MAP.addr_context,
        MEMORY_COL_MAP.addr_segment,
        MEMORY_COL_MAP.addr_virtual,
    ])
    .collect_vec();
    res.extend(Column::singles(MEMORY_COL_MAP.value_limbs));
    res.push(Column::single(MEMORY_COL_MAP.timestamp));
    res
}

/// CTL filter for memory operations.
pub(crate) fn ctl_filter<F: Field>() -> Filter<F> {
    Filter::new_simple(Column::single(MEMORY_COL_MAP.filter))
}

/// Creates the vector of `Columns` corresponding to:
/// - the initialized address (context, segment, virt),
/// - the value in u32 limbs.
pub(crate) fn ctl_looking_mem<F: Field>() -> Vec<Column<F>> {
    let mut res = Column::singles([
        MEMORY_COL_MAP.addr_context,
        MEMORY_COL_MAP.addr_segment,
        MEMORY_COL_MAP.addr_virtual,
    ])
    .collect_vec();
    res.extend(Column::singles(MEMORY_COL_MAP.value_limbs));
    res
}

/// Returns the (non-zero) stale contexts.
pub(crate) fn ctl_context_pruning_looking<F: Field>() -> TableWithColumns<F> {
    TableWithColumns::new(
        *Table::Memory,
        vec![Column::linear_combination_with_constant(
            vec![(MEMORY_COL_MAP.stale_contexts, F::ONE)],
            F::NEG_ONE,
        )],
        Filter::new(vec![], vec![Column::single(MEMORY_COL_MAP.is_pruned)]),
    )
}

/// CTL filter for initialization writes.
/// Initialization operations have timestamp 0.
/// The filter is `1 - timestamp * timestamp_inv`.
pub(crate) fn ctl_filter_mem_before<F: Field>() -> Filter<F> {
    Filter::new(
        vec![(
            Column::single(MEMORY_COL_MAP.timestamp),
            Column::linear_combination([(MEMORY_COL_MAP.timestamp_inv, -F::ONE)]),
        )],
        vec![Column::constant(F::ONE)],
    )
}

/// CTL filter for final values.
/// Final values are the last row with a given address.
/// The filter is `address_changed`.
pub(crate) fn ctl_filter_mem_after<F: Field>() -> Filter<F> {
    Filter::new_simple(Column::single(MEMORY_COL_MAP.mem_after_filter))
}

#[derive(Copy, Clone, Default)]
pub(crate) struct MemoryStark<F, const D: usize> {
    pub(crate) f: PhantomData<F>,
}

impl MemoryOp {
    /// Generate a row for a given memory operation. Note that this does not
    /// generate columns which depend on the next operation, such as
    /// `context_first_change`; those are generated later. It also does not
    /// generate columns such as `counter`, which are generated later, after the
    /// trace has been transposed into column-major form.
    fn into_row<F: Field>(self) -> MemoryColumnsView<F> {
        let mut row = MemoryColumnsView::default();
        row.filter = F::from_bool(self.filter);
        row.timestamp = F::from_canonical_usize(self.timestamp);
        row.timestamp_inv = row.timestamp.try_inverse().unwrap_or_default();
        row.is_read = F::from_bool(self.kind == Read);
        let MemoryAddress {
            context,
            segment,
            virt,
        } = self.address;
        row.addr_context = F::from_canonical_usize(context);
        row.addr_segment = F::from_canonical_usize(segment);
        row.addr_virtual = F::from_canonical_usize(virt);
        for j in 0..VALUE_LIMBS {
            row.value_limbs[j] = F::from_canonical_u32((self.value >> (j * 32)).low_u32());
        }

        row
    }
}

/// Generates the `*_first_change` columns and the `range_check` column in the
/// trace.
pub(crate) fn generate_first_change_flags_and_rc<F: RichField>(
    trace_rows: &mut [MemoryColumnsView<F>],
) {
    let num_ops = trace_rows.len();
    for idx in 0..num_ops {
        let row = &trace_rows[idx];
        let next_row = if idx == num_ops - 1 {
            &trace_rows[0]
        } else {
            &trace_rows[idx + 1]
        };

        let context = row.addr_context;
        let segment = row.addr_segment;
        let virt = row.addr_virtual;
        let timestamp = row.timestamp;
        let next_context = next_row.addr_context;
        let next_segment = next_row.addr_segment;
        let next_virt = next_row.addr_virtual;
        let next_timestamp = next_row.timestamp;
        let next_is_read = next_row.is_read;

        let context_changed = context != next_context;
        let segment_changed = segment != next_segment;
        let virtual_changed = virt != next_virt;

        let context_first_change = context_changed;
        let segment_first_change = segment_changed && !context_first_change;
        let virtual_first_change =
            virtual_changed && !segment_first_change && !context_first_change;

        let row = &mut trace_rows[idx];
        row.context_first_change = F::from_bool(context_first_change);
        row.segment_first_change = F::from_bool(segment_first_change);
        row.virtual_first_change = F::from_bool(virtual_first_change);

        row.range_check = if idx == num_ops - 1 {
            F::ZERO
        } else if context_first_change {
            next_context - context - F::ONE
        } else if segment_first_change {
            next_segment - segment - F::ONE
        } else if virtual_first_change {
            next_virt - virt - F::ONE
        } else {
            next_timestamp - timestamp
        };

        assert!(
            row.range_check.to_canonical_u64() < num_ops as u64,
            "Range check of {} is too large. Bug in fill_gaps?",
            row.range_check
        );

        row.preinitialized_segments_aux = (next_segment
            - F::from_canonical_usize(Segment::AccountsLinkedList.unscale()))
            * (next_segment - F::from_canonical_usize(Segment::StorageLinkedList.unscale()));

        row.preinitialized_segments = (next_segment
            - F::from_canonical_usize(Segment::Code.unscale()))
            * (next_segment - F::from_canonical_usize(Segment::TrieData.unscale()))
            * row.preinitialized_segments_aux;

        let address_changed =
            row.context_first_change + row.segment_first_change + row.virtual_first_change;
        row.initialize_aux = row.preinitialized_segments * address_changed * next_is_read;
    }
}

impl<F: RichField + Extendable<D>, const D: usize> MemoryStark<F, D> {
    /// Generate most of the trace rows. Excludes a few columns like `counter`,
    /// which are generated later, after transposing to column-major form.
    fn generate_trace_row_major(
        &self,
        mut memory_ops: Vec<MemoryOp>,
    ) -> (Vec<MemoryColumnsView<F>>, usize) {
        // fill_gaps expects an ordered list of operations.
        memory_ops.sort_by_key(MemoryOp::sorting_key);
        Self::fill_gaps(&mut memory_ops);

        let unpadded_length = memory_ops.len();

        memory_ops.sort_by_key(MemoryOp::sorting_key);

        Self::pad_memory_ops(&mut memory_ops);

        // fill_gaps may have added operations at the end which break the order, so sort
        // again.
        memory_ops.sort_by_key(MemoryOp::sorting_key);

        let mut trace_rows = memory_ops
            .into_par_iter()
            .map(|op| op.into_row())
            .collect::<Vec<_>>();
        generate_first_change_flags_and_rc(&mut trace_rows);
        (trace_rows, unpadded_length)
    }

    /// Generates the `counter`, `range_check` and `frequencies` columns, given
    /// a trace in column-major form.
    /// Also generates the `state_contexts`, `state_contexts_frequencies`,
    /// `maybe_in_mem_after` and `mem_after_filter` columns.
    fn generate_trace_col_major(trace_col_vecs: &mut [Vec<F>]) {
        let height = trace_col_vecs[0].len();
        trace_col_vecs[MEMORY_COL_MAP.counter] =
            (0..height).map(|i| F::from_canonical_usize(i)).collect();

        for i in 0..height {
            let x_rc = trace_col_vecs[MEMORY_COL_MAP.range_check][i].to_canonical_u64() as usize;
            trace_col_vecs[MEMORY_COL_MAP.frequencies][x_rc] += F::ONE;
            if (trace_col_vecs[MEMORY_COL_MAP.context_first_change][i] == F::ONE)
                || (trace_col_vecs[MEMORY_COL_MAP.segment_first_change][i] == F::ONE)
            {
                if i < trace_col_vecs[MEMORY_COL_MAP.addr_virtual].len() - 1 {
                    let x_val = trace_col_vecs[MEMORY_COL_MAP.addr_virtual][i + 1]
                        .to_canonical_u64() as usize;
                    trace_col_vecs[MEMORY_COL_MAP.frequencies][x_val] += F::ONE;
                } else {
                    trace_col_vecs[MEMORY_COL_MAP.frequencies][0] += F::ONE;
                }
            }

            let addr_ctx = trace_col_vecs[MEMORY_COL_MAP.addr_context][i];
            let addr_ctx_usize = addr_ctx.to_canonical_u64() as usize;
            if addr_ctx + F::ONE == trace_col_vecs[MEMORY_COL_MAP.stale_contexts][addr_ctx_usize] {
                trace_col_vecs[MEMORY_COL_MAP.is_stale][i] = F::ONE;
                trace_col_vecs[MEMORY_COL_MAP.stale_context_frequencies][addr_ctx_usize] += F::ONE;
            } else if trace_col_vecs[MEMORY_COL_MAP.filter][i].is_one()
                && (trace_col_vecs[MEMORY_COL_MAP.context_first_change][i].is_one()
                    || trace_col_vecs[MEMORY_COL_MAP.segment_first_change][i].is_one()
                    || trace_col_vecs[MEMORY_COL_MAP.virtual_first_change][i].is_one())
            {
                // `maybe_in_mem_after = filter * address_changed * (1 - is_stale)`
                trace_col_vecs[MEMORY_COL_MAP.maybe_in_mem_after][i] = F::ONE;

                let addr_segment = trace_col_vecs[MEMORY_COL_MAP.addr_segment][i];
                let is_non_zero_value = (0..VALUE_LIMBS)
                    .any(|limb| trace_col_vecs[MEMORY_COL_MAP.value_limbs[limb]][i].is_nonzero());
                // We filter out zero values in non-preinitialized segments.
                if is_non_zero_value
                    || PREINITIALIZED_SEGMENTS_INDICES
                        .contains(&(addr_segment.to_canonical_u64() as usize))
                {
                    trace_col_vecs[MEMORY_COL_MAP.mem_after_filter][i] = F::ONE;
                }
            }
        }
    }

    /// This memory STARK orders rows by `(context, segment, virt, timestamp)`.
    /// To enforce the ordering, it range checks the delta of the first
    /// field that changed.
    ///
    /// This method adds some dummy operations to ensure that none of these
    /// range checks will be too large, i.e. that they will all be smaller
    /// than the number of rows, allowing them to be checked easily with a
    /// single lookup.
    ///
    /// For example, say there are 32 memory operations, and a particular
    /// address is accessed at timestamps 20 and 100. 80 would fail the
    /// range check, so this method would add two dummy reads to the same
    /// address, say at timestamps 50 and 80.
    fn fill_gaps(memory_ops: &mut Vec<MemoryOp>) {
        // First, insert padding row at address (0, 0, 0) if the first row doesn't
        // have a first virtual address at 0.
        if memory_ops[0].address.virt != 0 {
            let dummy_addr = MemoryAddress {
                context: 0,
                segment: 0,
                virt: 0,
            };
            memory_ops.insert(
                0,
                MemoryOp {
                    filter: false,
                    timestamp: 1,
                    address: dummy_addr,
                    kind: MemoryOpKind::Read,
                    value: 0.into(),
                },
            );
        }
        let max_rc = memory_ops.len().next_power_of_two() - 1;
        for (mut curr, mut next) in memory_ops.clone().into_iter().tuple_windows() {
            if curr.address.context != next.address.context
                || curr.address.segment != next.address.segment
            {
                // We won't bother to check if there's a large context gap, because there can't
                // be more than 500 contexts or so, as explained here:
                // https://notes.ethereum.org/@vbuterin/proposals_to_adjust_memory_gas_costs
                // Similarly, the number of possible segments is a small constant, so any gap
                // must be small. max_rc will always be much larger, as just
                // bootloading the kernel will trigger thousands of memory
                // operations. However, we do check that the first address
                // accessed is range-checkable. If not, we could start at a
                // negative address and cheat.
                while next.address.virt > max_rc {
                    let mut dummy_address = next.address;
                    dummy_address.virt -= max_rc;
                    let dummy_read =
                        MemoryOp::new_dummy_read(dummy_address, curr.timestamp + 1, U256::zero());
                    memory_ops.push(dummy_read);
                    next = dummy_read;
                }
            } else if curr.address.virt != next.address.virt {
                while next.address.virt - curr.address.virt - 1 > max_rc {
                    let mut dummy_address = curr.address;
                    dummy_address.virt += max_rc + 1;
                    let dummy_read =
                        MemoryOp::new_dummy_read(dummy_address, curr.timestamp + 1, U256::zero());
                    memory_ops.push(dummy_read);
                    curr = dummy_read;
                }
            } else {
                while next.timestamp - curr.timestamp > max_rc {
                    let dummy_read =
                        MemoryOp::new_dummy_read(curr.address, curr.timestamp + max_rc, curr.value);
                    memory_ops.push(dummy_read);
                    curr = dummy_read;
                }
            }
        }
    }

    fn pad_memory_ops(memory_ops: &mut Vec<MemoryOp>) {
        let last_op = *memory_ops.last().expect("No memory ops?");

        // We essentially repeat the last operation until our operation list has the
        // desired size, with a few changes:
        // - We change its filter to 0 to indicate that this is a dummy operation.
        // - We make sure it's a read, since dummy operations must be reads.
        // - We change the address so that the last operation can still be included in
        //   `MemAfterStark`.
        let padding_addr = MemoryAddress {
            virt: last_op.address.virt + 1,
            ..last_op.address
        };
        let padding_op = MemoryOp {
            filter: false,
            kind: Read,
            address: padding_addr,
            timestamp: last_op.timestamp + 1,
            value: U256::zero(),
        };
        let num_ops = memory_ops.len();
        // We want at least one padding row, so that the last real operation can have
        // its flags set correctly.
        let num_ops_padded = (num_ops + 1).next_power_of_two();
        for _ in num_ops..num_ops_padded {
            memory_ops.push(padding_op);
        }
    }

    fn insert_stale_contexts(trace_rows: &mut [MemoryColumnsView<F>], stale_contexts: Vec<usize>) {
        debug_assert!(
            {
                let mut dedup_vec = stale_contexts.clone();
                dedup_vec.sort();
                dedup_vec.dedup();
                dedup_vec.len() == stale_contexts.len()
            },
            "Stale contexts are not unique.",
        );

        for ctx in stale_contexts {
            let ctx_field = F::from_canonical_usize(ctx);
            // We store `ctx_field+1` so that 0 can be the default value for non-stale
            // context.
            trace_rows[ctx].stale_contexts = ctx_field + F::ONE;
            trace_rows[ctx].is_pruned = F::ONE;
        }
    }

    pub(crate) fn generate_trace(
        &self,
        mut memory_ops: Vec<MemoryOp>,
        mem_before_values: &[(MemoryAddress, U256)],
        stale_contexts: Vec<usize>,
        timing: &mut TimingTree,
    ) -> (Vec<PolynomialValues<F>>, Vec<Vec<F>>, usize) {
        // First, push `mem_before` operations.
        for &(address, value) in mem_before_values {
            memory_ops.push(MemoryOp {
                filter: true,
                timestamp: 0,
                address,
                kind: crate::witness::memory::MemoryOpKind::Write,
                value,
            });
        }
        // Generate most of the trace in row-major form.
        let (mut trace_rows, unpadded_length) = timed!(
            timing,
            "generate trace rows",
            self.generate_trace_row_major(memory_ops)
        );

        Self::insert_stale_contexts(&mut trace_rows, stale_contexts.clone());

        let trace_row_vecs: Vec<_> = trace_rows.into_iter().map(|row| row.to_vec()).collect();

        // Transpose to column-major form.
        let mut trace_col_vecs = transpose(&trace_row_vecs);

        // A few final generation steps, which work better in column-major form.
        Self::generate_trace_col_major(&mut trace_col_vecs);

        let final_rows = transpose(&trace_col_vecs);

        // Extract `MemoryAfterStark` values.
        let mut mem_after_values = Vec::<Vec<_>>::new();
        for row in final_rows {
            if row[MEMORY_COL_MAP.mem_after_filter].is_one() {
                let mut addr_val = vec![F::ONE];
                addr_val
                    .extend(&row[MEMORY_COL_MAP.addr_context..MEMORY_COL_MAP.context_first_change]);
                mem_after_values.push(addr_val);
            }
        }

        (
            trace_col_vecs
                .into_iter()
                .map(|column| PolynomialValues::new(column))
                .collect(),
            mem_after_values,
            unpadded_length,
        )
    }
}

impl<F: RichField + Extendable<D>, const D: usize> Stark<F, D> for MemoryStark<F, D> {
    type EvaluationFrame<FE, P, const D2: usize> = EvmStarkFrame<P, FE, NUM_COLUMNS>
    where
        FE: FieldExtension<D2, BaseField = F>,
        P: PackedField<Scalar = FE>;

    type EvaluationFrameTarget = EvmStarkFrame<ExtensionTarget<D>, ExtensionTarget<D>, NUM_COLUMNS>;

    fn eval_packed_generic<FE, P, const D2: usize>(
        &self,
        vars: &Self::EvaluationFrame<FE, P, D2>,
        yield_constr: &mut ConstraintConsumer<P>,
    ) where
        FE: FieldExtension<D2, BaseField = F>,
        P: PackedField<Scalar = FE>,
    {
        let one = P::from(FE::ONE);

        let lv: &[P; NUM_COLUMNS] = vars.get_local_values().try_into().unwrap();
        let lv: &MemoryColumnsView<P> = lv.borrow();
        let nv: &[P; NUM_COLUMNS] = vars.get_next_values().try_into().unwrap();
        let nv: &MemoryColumnsView<P> = nv.borrow();

        let timestamp = lv.timestamp;
        let addr_context = lv.addr_context;
        let addr_segment = lv.addr_segment;
        let addr_virtual = lv.addr_virtual;
        let value_limbs = lv.value_limbs;
        let timestamp_inv = lv.timestamp_inv;
        let is_stale = lv.is_stale;
        let maybe_in_mem_after = lv.maybe_in_mem_after;
        let mem_after_filter = lv.mem_after_filter;
        let initialize_aux = lv.initialize_aux;
        let preinitialized_segments = lv.preinitialized_segments;
        let preinitialized_segments_aux = lv.preinitialized_segments_aux;

        let next_timestamp = nv.timestamp;
        let next_is_read = nv.is_read;
        let next_addr_context = nv.addr_context;
        let next_addr_segment = nv.addr_segment;
        let next_addr_virtual = nv.addr_virtual;
        let next_values_limbs = nv.value_limbs;

        // The filter must be 0 or 1.
        let filter = lv.filter;
        yield_constr.constraint(filter * (filter - P::ONES));

        // IS_READ must be 0 or 1.
        // This is implied by the MemoryStark CTL, where corresponding values are either
        // hardcoded to 0/1, or boolean-constrained in their respective STARK modules.

        // If this is a dummy row (filter is off), it must be a read. This means the
        // prover can insert reads which never appear in the CPU trace (which
        // are harmless), but not writes.
        let is_dummy = P::ONES - filter;
        let is_write = P::ONES - lv.is_read;
        yield_constr.constraint(is_dummy * is_write);

        let context_first_change = lv.context_first_change;
        let segment_first_change = lv.segment_first_change;
        let virtual_first_change = lv.virtual_first_change;
        let address_unchanged =
            one - context_first_change - segment_first_change - virtual_first_change;

        let range_check = lv.range_check;

        let not_context_first_change = one - context_first_change;
        let not_segment_first_change = one - segment_first_change;
        let not_virtual_first_change = one - virtual_first_change;
        let not_address_unchanged = one - address_unchanged;

        // First set of ordering constraint: first_change flags are boolean.
        yield_constr.constraint(context_first_change * not_context_first_change);
        yield_constr.constraint(segment_first_change * not_segment_first_change);
        yield_constr.constraint(virtual_first_change * not_virtual_first_change);
        yield_constr.constraint(address_unchanged * not_address_unchanged);

        // Second set of ordering constraints: no change before the column corresponding
        // to the nonzero first_change flag.
        yield_constr
            .constraint_transition(segment_first_change * (next_addr_context - addr_context));
        yield_constr
            .constraint_transition(virtual_first_change * (next_addr_context - addr_context));
        yield_constr
            .constraint_transition(virtual_first_change * (next_addr_segment - addr_segment));
        yield_constr.constraint_transition(address_unchanged * (next_addr_context - addr_context));
        yield_constr.constraint_transition(address_unchanged * (next_addr_segment - addr_segment));
        yield_constr.constraint_transition(address_unchanged * (next_addr_virtual - addr_virtual));

        // Third set of ordering constraints: range-check difference in the column that
        // should be increasing.
        let computed_range_check = context_first_change * (next_addr_context - addr_context - one)
            + segment_first_change * (next_addr_segment - addr_segment - one)
            + virtual_first_change * (next_addr_virtual - addr_virtual - one)
            + address_unchanged * (next_timestamp - timestamp);
        yield_constr.constraint_transition(range_check - computed_range_check);

        // Validate `preinitialized_segments_aux`.
        yield_constr.constraint_transition(
            preinitialized_segments_aux
                - (next_addr_segment
                    - P::Scalar::from_canonical_usize(Segment::AccountsLinkedList.unscale()))
                    * (next_addr_segment
                        - P::Scalar::from_canonical_usize(Segment::StorageLinkedList.unscale())),
        );

        // Validate `preinitialized_segments`.
        yield_constr.constraint_transition(
            preinitialized_segments
                - (next_addr_segment - P::Scalar::from_canonical_usize(Segment::Code.unscale()))
                    * (next_addr_segment
                        - P::Scalar::from_canonical_usize(Segment::TrieData.unscale()))
                    * preinitialized_segments_aux,
        );

        // Validate `initialize_aux`.
        yield_constr.constraint_transition(
            initialize_aux - preinitialized_segments * not_address_unchanged * next_is_read,
        );

        for i in 0..VALUE_LIMBS {
            // Enumerate purportedly-ordered log.
            yield_constr.constraint_transition(
                next_is_read * address_unchanged * (next_values_limbs[i] - value_limbs[i]),
            );
            // By default, memory is initialized with 0. This means that if the first
            // operation of a new address is a read, then its value must be 0.
            // There are exceptions, though: this constraint zero-initializes everything but
            // the preinitialized segments.
            yield_constr.constraint_transition(initialize_aux * next_values_limbs[i]);
        }

        // Validate `maybe_in_mem_after`.
        yield_constr.constraint_transition(
            maybe_in_mem_after + filter * not_address_unchanged * (is_stale - P::ONES),
        );

        // `mem_after_filter` must be binary.
        yield_constr.constraint(mem_after_filter * (mem_after_filter - P::ONES));

        // `mem_after_filter` is equal to `maybe_in_mem_after` if:
        // - segment is not preinitialized OR
        // - value is not zero.
        for i in 0..VALUE_LIMBS {
            yield_constr.constraint(
                (mem_after_filter - maybe_in_mem_after) * preinitialized_segments * value_limbs[i],
            );
        }

        // Validate timestamp_inv. Since it's used as a CTL filter, its value must be
        // checked.
        yield_constr.constraint(timestamp * (timestamp * timestamp_inv - P::ONES));

        // Check the range column: First value must be 0,
        // and intermediate rows must increment by 1.
        let rc1 = lv.counter;
        let rc2 = nv.counter;
        yield_constr.constraint_first_row(rc1);
        let incr = rc2 - rc1;
        yield_constr.constraint_transition(incr - P::Scalar::ONES);
    }

    fn eval_ext_circuit(
        &self,
        builder: &mut plonky2::plonk::circuit_builder::CircuitBuilder<F, D>,
        vars: &Self::EvaluationFrameTarget,
        yield_constr: &mut RecursiveConstraintConsumer<F, D>,
    ) {
        let one = builder.one_extension();

        let lv: &[ExtensionTarget<D>; NUM_COLUMNS] = vars.get_local_values().try_into().unwrap();
        let lv: &MemoryColumnsView<ExtensionTarget<D>> = lv.borrow();
        let nv: &[ExtensionTarget<D>; NUM_COLUMNS] = vars.get_next_values().try_into().unwrap();
        let nv: &MemoryColumnsView<ExtensionTarget<D>> = nv.borrow();

        let addr_context = lv.addr_context;
        let addr_segment = lv.addr_segment;
        let addr_virtual = lv.addr_virtual;
        let value_limbs = lv.value_limbs;
        let timestamp = lv.timestamp;
        let timestamp_inv = lv.timestamp_inv;
        let is_stale = lv.is_stale;
        let maybe_in_mem_after = lv.maybe_in_mem_after;
        let mem_after_filter = lv.mem_after_filter;
        let initialize_aux = lv.initialize_aux;
        let preinitialized_segments = lv.preinitialized_segments;
        let preinitialized_segments_aux = lv.preinitialized_segments_aux;

        let next_addr_context = nv.addr_context;
        let next_addr_segment = nv.addr_segment;
        let next_addr_virtual = nv.addr_virtual;
        let next_values_limbs = nv.value_limbs;
        let next_is_read = nv.is_read;
        let next_timestamp = nv.timestamp;

        // The filter must be 0 or 1.
        let filter = lv.filter;
        let constraint = builder.mul_sub_extension(filter, filter, filter);
        yield_constr.constraint(builder, constraint);

        // IS_READ must be 0 or 1.
        // This is implied by the MemoryStark CTL, where corresponding values are either
        // hardcoded to 0/1, or boolean-constrained in their respective STARK modules.

        // If this is a dummy row (filter is off), it must be a read. This means the
        // prover can insert reads which never appear in the CPU trace (which
        // are harmless), but not writes.
        let is_dummy = builder.sub_extension(one, filter);
        let is_write = builder.sub_extension(one, lv.is_read);
        let is_dummy_write = builder.mul_extension(is_dummy, is_write);
        yield_constr.constraint(builder, is_dummy_write);

        let context_first_change = lv.context_first_change;
        let segment_first_change = lv.segment_first_change;
        let virtual_first_change = lv.virtual_first_change;
        let address_unchanged = {
            let mut cur = builder.sub_extension(one, context_first_change);
            cur = builder.sub_extension(cur, segment_first_change);
            builder.sub_extension(cur, virtual_first_change)
        };

        let range_check = lv.range_check;

        let not_context_first_change = builder.sub_extension(one, context_first_change);
        let not_segment_first_change = builder.sub_extension(one, segment_first_change);
        let not_virtual_first_change = builder.sub_extension(one, virtual_first_change);
        let not_address_unchanged = builder.sub_extension(one, address_unchanged);
        let addr_context_diff = builder.sub_extension(next_addr_context, addr_context);
        let addr_segment_diff = builder.sub_extension(next_addr_segment, addr_segment);
        let addr_virtual_diff = builder.sub_extension(next_addr_virtual, addr_virtual);

        // First set of ordering constraint: traces are boolean.
        let context_first_change_bool =
            builder.mul_extension(context_first_change, not_context_first_change);
        yield_constr.constraint(builder, context_first_change_bool);
        let segment_first_change_bool =
            builder.mul_extension(segment_first_change, not_segment_first_change);
        yield_constr.constraint(builder, segment_first_change_bool);
        let virtual_first_change_bool =
            builder.mul_extension(virtual_first_change, not_virtual_first_change);
        yield_constr.constraint(builder, virtual_first_change_bool);
        let address_unchanged_bool =
            builder.mul_extension(address_unchanged, not_address_unchanged);
        yield_constr.constraint(builder, address_unchanged_bool);

        // Second set of ordering constraints: no change before the column corresponding
        // to the nonzero first_change flag.
        let segment_first_change_check =
            builder.mul_extension(segment_first_change, addr_context_diff);
        yield_constr.constraint_transition(builder, segment_first_change_check);
        let virtual_first_change_check_1 =
            builder.mul_extension(virtual_first_change, addr_context_diff);
        yield_constr.constraint_transition(builder, virtual_first_change_check_1);
        let virtual_first_change_check_2 =
            builder.mul_extension(virtual_first_change, addr_segment_diff);
        yield_constr.constraint_transition(builder, virtual_first_change_check_2);
        let address_unchanged_check_1 = builder.mul_extension(address_unchanged, addr_context_diff);
        yield_constr.constraint_transition(builder, address_unchanged_check_1);
        let address_unchanged_check_2 = builder.mul_extension(address_unchanged, addr_segment_diff);
        yield_constr.constraint_transition(builder, address_unchanged_check_2);
        let address_unchanged_check_3 = builder.mul_extension(address_unchanged, addr_virtual_diff);
        yield_constr.constraint_transition(builder, address_unchanged_check_3);

        // Third set of ordering constraints: range-check difference in the column that
        // should be increasing.
        let context_diff = {
            let diff = builder.sub_extension(next_addr_context, addr_context);
            builder.sub_extension(diff, one)
        };
        let segment_diff = {
            let diff = builder.sub_extension(next_addr_segment, addr_segment);
            builder.sub_extension(diff, one)
        };
        let segment_range_check = builder.mul_extension(segment_first_change, segment_diff);
        let virtual_diff = {
            let diff = builder.sub_extension(next_addr_virtual, addr_virtual);
            builder.sub_extension(diff, one)
        };
        let virtual_range_check = builder.mul_extension(virtual_first_change, virtual_diff);
        let timestamp_diff = builder.sub_extension(next_timestamp, timestamp);
        let timestamp_range_check = builder.mul_extension(address_unchanged, timestamp_diff);

        let computed_range_check = {
            // context_range_check = context_first_change * context_diff
            let mut sum =
                builder.mul_add_extension(context_first_change, context_diff, segment_range_check);
            sum = builder.add_extension(sum, virtual_range_check);
            builder.add_extension(sum, timestamp_range_check)
        };
        let range_check_diff = builder.sub_extension(range_check, computed_range_check);
        yield_constr.constraint_transition(builder, range_check_diff);

        // Validate `preinitialized_segments_aux`.
        let segment_accounts_list = builder.add_const_extension(
            next_addr_segment,
            -F::from_canonical_usize(Segment::AccountsLinkedList.unscale()),
        );
        let segment_storage_list = builder.add_const_extension(
            next_addr_segment,
            -F::from_canonical_usize(Segment::StorageLinkedList.unscale()),
        );
        let segment_aux_prod = builder.mul_extension(segment_accounts_list, segment_storage_list);
        let preinitialized_segments_aux_constraint =
            builder.sub_extension(preinitialized_segments_aux, segment_aux_prod);
        yield_constr.constraint_transition(builder, preinitialized_segments_aux_constraint);

        // Validate `preinitialized_segments`.
        let segment_code = builder.add_const_extension(
            next_addr_segment,
            -F::from_canonical_usize(Segment::Code.unscale()),
        );
        let segment_trie_data = builder.add_const_extension(
            next_addr_segment,
            -F::from_canonical_usize(Segment::TrieData.unscale()),
        );

        let segment_prod = builder.mul_many_extension([
            segment_code,
            segment_trie_data,
            preinitialized_segments_aux,
        ]);
        let preinitialized_segments_constraint =
            builder.sub_extension(preinitialized_segments, segment_prod);
        yield_constr.constraint_transition(builder, preinitialized_segments_constraint);

        // Validate `initialize_aux`.
        let computed_initialize_aux = builder.mul_extension(not_address_unchanged, next_is_read);
        let computed_initialize_aux =
            builder.mul_extension(preinitialized_segments, computed_initialize_aux);
        let new_first_read_constraint =
            builder.sub_extension(initialize_aux, computed_initialize_aux);
        yield_constr.constraint_transition(builder, new_first_read_constraint);

        for i in 0..VALUE_LIMBS {
            // Enumerate purportedly-ordered log.
            let value_diff = builder.sub_extension(next_values_limbs[i], value_limbs[i]);
            let zero_if_read = builder.mul_extension(address_unchanged, value_diff);
            let read_constraint = builder.mul_extension(next_is_read, zero_if_read);
            yield_constr.constraint_transition(builder, read_constraint);
            // By default, memory is initialized with 0. This means that if the first
            // operation of a new address is a read, then its value must be 0.
            // There are exceptions, though: this constraint zero-initializes everything but
            // the preinitialized segments.
            let zero_init_constraint = builder.mul_extension(initialize_aux, next_values_limbs[i]);
            yield_constr.constraint_transition(builder, zero_init_constraint);
        }

        // Validate `maybe_in_mem_after`.
        {
            let rhs = builder.mul_extension(filter, not_address_unchanged);
            let rhs = builder.mul_sub_extension(rhs, is_stale, rhs);
            let constr = builder.add_extension(maybe_in_mem_after, rhs);
            yield_constr.constraint_transition(builder, constr);
        }

        // `mem_after_filter` must be binary.
        {
            let constr =
                builder.mul_sub_extension(mem_after_filter, mem_after_filter, mem_after_filter);
            yield_constr.constraint(builder, constr);
        }

        // `mem_after_filter` is equal to `maybe_in_mem_after` if:
        // - segment is not preinitialized OR
        // - value is not zero.
        let mem_after_filter_diff = builder.sub_extension(mem_after_filter, maybe_in_mem_after);
        for i in 0..VALUE_LIMBS {
            let prod = builder.mul_extension(preinitialized_segments, value_limbs[i]);
            let constr = builder.mul_extension(mem_after_filter_diff, prod);
            yield_constr.constraint(builder, constr);
        }

        // Validate timestamp_inv. Since it's used as a CTL filter, its value must be
        // checked.
        let timestamp_prod = builder.mul_extension(timestamp, timestamp_inv);
        let timestamp_inv_constraint =
            builder.mul_sub_extension(timestamp, timestamp_prod, timestamp);
        yield_constr.constraint(builder, timestamp_inv_constraint);

        // Check the range column: First value must be 0,
        // and intermediate rows must increment by 1.
        let rc1 = lv.counter;
        let rc2 = nv.counter;
        yield_constr.constraint_first_row(builder, rc1);
        let incr = builder.sub_extension(rc2, rc1);
        let t = builder.sub_extension(incr, one);
        yield_constr.constraint_transition(builder, t);
    }

    fn constraint_degree(&self) -> usize {
        3
    }

    fn lookups(&self) -> Vec<Lookup<F>> {
        vec![
            Lookup {
                columns: vec![
                    Column::single(MEMORY_COL_MAP.range_check),
                    Column::single_next_row(MEMORY_COL_MAP.addr_virtual),
                ],
                table_column: Column::single(MEMORY_COL_MAP.counter),
                frequencies_column: Column::single(MEMORY_COL_MAP.frequencies),
                filter_columns: vec![
                    Default::default(),
                    Filter::new_simple(Column::sum([
                        MEMORY_COL_MAP.context_first_change,
                        MEMORY_COL_MAP.segment_first_change,
                    ])),
                ],
            },
            Lookup {
                columns: vec![Column::linear_combination_with_constant(
                    vec![(MEMORY_COL_MAP.addr_context, F::ONE)],
                    F::ONE,
                )],
                table_column: Column::single(MEMORY_COL_MAP.stale_contexts),
                frequencies_column: Column::single(MEMORY_COL_MAP.stale_context_frequencies),
                filter_columns: vec![Filter::new_simple(Column::single(MEMORY_COL_MAP.is_stale))],
            },
        ]
    }

    fn requires_ctls(&self) -> bool {
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use anyhow::Result;
    use plonky2::plonk::config::{GenericConfig, PoseidonGoldilocksConfig};
    use starky::stark_testing::{test_stark_circuit_constraints, test_stark_low_degree};

    use crate::memory::memory_stark::MemoryStark;

    #[test]
    fn test_stark_degree() -> Result<()> {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        type S = MemoryStark<F, D>;

        let stark = S {
            f: Default::default(),
        };
        test_stark_low_degree(stark)
    }

    #[test]
    fn test_stark_circuit() -> Result<()> {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        type S = MemoryStark<F, D>;

        let stark = S {
            f: Default::default(),
        };
        test_stark_circuit_constraints::<F, C, S, D>(stark)
    }
}
