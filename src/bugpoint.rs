//! CLI tool to reduce Cranelift IR files crashing during compilation.

use crate::disasm::{PrintRelocs, PrintStackmaps, PrintTraps};
use crate::utils::{parse_sets_and_triple, read_to_string};
use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::types::{F32, F64};
use cranelift_codegen::ir::{
    self, Ebb, FuncRef, Function, GlobalValueData, Inst, InstBuilder, InstructionData, StackSlots,
    TrapCode,
};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::Context;
use cranelift_entity::PrimaryMap;
use cranelift_reader::{parse_test, ParseOptions};
use std::collections::HashMap;
use std::path::Path;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub fn run(
    filename: &str,
    flag_set: &[String],
    flag_isa: &str,
    verbose: bool,
) -> Result<(), String> {
    let parsed = parse_sets_and_triple(flag_set, flag_isa)?;
    let fisa = parsed.as_fisa();

    let path = Path::new(&filename).to_path_buf();

    let buffer = read_to_string(&path).map_err(|e| format!("{}: {}", filename, e))?;
    let test_file =
        parse_test(&buffer, ParseOptions::default()).map_err(|e| format!("{}: {}", filename, e))?;

    // If we have an isa from the command-line, use that. Otherwise if the
    // file contains a unique isa, use that.
    let isa = if let Some(isa) = fisa.isa {
        isa
    } else if let Some(isa) = test_file.isa_spec.unique_isa() {
        isa
    } else {
        return Err(String::from("compilation requires a target isa"));
    };

    std::env::set_var("RUST_BACKTRACE", "0"); // Disable backtraces to reduce verbosity

    for (func, _) in test_file.functions {
        let (orig_ebb_count, orig_inst_count) = (ebb_count(&func), inst_count(&func));

        match reduce(isa, func, verbose) {
            Ok((func, crash_msg)) => {
                println!("Crash message: {}", crash_msg);
                println!("\n{}", func);
                println!(
                    "{} ebbs {} insts -> {} ebbs {} insts",
                    orig_ebb_count,
                    orig_inst_count,
                    ebb_count(&func),
                    inst_count(&func)
                );
            }
            Err(err) => println!("Warning: {}", err),
        }
    }

    Ok(())
}

enum ProgressStatus {
    /// The mutation raised or reduced the amount of instructions or ebbs.
    ExpandedOrShrinked,

    /// The mutation only changed an instruction. Performing another round of mutations may only
    /// reduce the test case if another mutation shrank the test case.
    Changed,

    /// No need to re-test if the program crashes, because the mutation had no effect, but we want
    /// to keep on iterating.
    Skip,
}

trait Mutator {
    fn name(&self) -> &'static str;
    fn mutation_count(&self, func: &Function) -> usize;
    fn mutate(&mut self, func: Function) -> Option<(Function, String, ProgressStatus)>;

    /// Gets called when the returned mutated function kept on causing the crash. This can be used
    /// to update position of the next item to look at. Does nothing by default.
    fn did_crash(&mut self) {}
}

/// Try to remove instructions.
struct RemoveInst {
    ebb: Ebb,
    inst: Inst,
}

impl RemoveInst {
    fn new(func: &Function) -> Self {
        let first_ebb = func.layout.entry_block().unwrap();
        let first_inst = func.layout.first_inst(first_ebb).unwrap();
        Self {
            ebb: first_ebb,
            inst: first_inst,
        }
    }
}

impl Mutator for RemoveInst {
    fn name(&self) -> &'static str {
        "remove inst"
    }

    fn mutation_count(&self, func: &Function) -> usize {
        inst_count(func)
    }

    fn mutate(&mut self, mut func: Function) -> Option<(Function, String, ProgressStatus)> {
        next_inst_ret_prev(&func, &mut self.ebb, &mut self.inst).map(|(prev_ebb, prev_inst)| {
            func.layout.remove_inst(prev_inst);
            let msg = if func.layout.ebb_insts(prev_ebb).next().is_none() {
                // Make sure empty ebbs are removed, as `next_inst_ret_prev` depends on non empty ebbs
                func.layout.remove_ebb(prev_ebb);
                format!("Remove inst {} and empty ebb {}", prev_inst, prev_ebb)
            } else {
                format!("Remove inst {}", prev_inst)
            };
            (func, msg, ProgressStatus::ExpandedOrShrinked)
        })
    }
}

/// Try to replace instructions with `iconst` or `fconst`.
struct ReplaceInstWithConst {
    ebb: Ebb,
    inst: Inst,
}

impl ReplaceInstWithConst {
    fn new(func: &Function) -> Self {
        let first_ebb = func.layout.entry_block().unwrap();
        let first_inst = func.layout.first_inst(first_ebb).unwrap();
        Self {
            ebb: first_ebb,
            inst: first_inst,
        }
    }

    fn const_for_type<'f, T: InstBuilder<'f>>(builder: T, ty: ir::Type) -> &'static str {
        // Try to keep the result type consistent, and default to an integer type
        // otherwise: this will cover all the cases for f32/f64 and integer types, or
        // create verifier errors otherwise.
        if ty == F32 {
            builder.f32const(0.0);
            "f32const"
        } else if ty == F64 {
            builder.f64const(0.0);
            "f64const"
        } else {
            builder.iconst(ty, 0);
            "iconst"
        }
    }
}

impl Mutator for ReplaceInstWithConst {
    fn name(&self) -> &'static str {
        "replace inst with const"
    }

    fn mutation_count(&self, func: &Function) -> usize {
        inst_count(func)
    }

    fn mutate(&mut self, mut func: Function) -> Option<(Function, String, ProgressStatus)> {
        next_inst_ret_prev(&func, &mut self.ebb, &mut self.inst).map(|(_prev_ebb, prev_inst)| {
            let num_results = func.dfg.inst_results(prev_inst).len();

            let opcode = func.dfg[prev_inst].opcode();
            if num_results == 0
                || opcode == ir::Opcode::Iconst
                || opcode == ir::Opcode::F32const
                || opcode == ir::Opcode::F64const
            {
                return (func, format!(""), ProgressStatus::Skip);
            }

            if num_results == 1 {
                let ty = func.dfg.value_type(func.dfg.first_result(prev_inst));
                let new_inst_name = Self::const_for_type(func.dfg.replace(prev_inst), ty);
                return (
                    func,
                    format!("Replace inst {} with {}.", prev_inst, new_inst_name),
                    ProgressStatus::Changed,
                );
            }

            // At least 2 results. Replace each instruction with as many const instructions as
            // there are results.
            let mut pos = FuncCursor::new(&mut func).at_inst(prev_inst);

            // Copy result SSA names into our own vector; otherwise we couldn't mutably borrow pos
            // in the loop below.
            let results = pos.func.dfg.inst_results(prev_inst).to_vec();

            // Detach results from the previous instruction, since we're going to reuse them.
            pos.func.dfg.clear_results(prev_inst);

            let mut inst_names = Vec::new();
            for r in results {
                let ty = pos.func.dfg.value_type(r);
                let builder = pos.ins().with_results([Some(r)]);
                let new_inst_name = Self::const_for_type(builder, ty);
                inst_names.push(new_inst_name);
            }

            // Remove the instruction.
            assert_eq!(pos.remove_inst(), prev_inst);

            (
                func,
                format!("Replace inst {} with {}", prev_inst, inst_names.join(" / ")),
                ProgressStatus::ExpandedOrShrinked,
            )
        })
    }
}

/// Try to replace instructions with `trap`.
struct ReplaceInstWithTrap {
    ebb: Ebb,
    inst: Inst,
}

impl ReplaceInstWithTrap {
    fn new(func: &Function) -> Self {
        let first_ebb = func.layout.entry_block().unwrap();
        let first_inst = func.layout.first_inst(first_ebb).unwrap();
        Self {
            ebb: first_ebb,
            inst: first_inst,
        }
    }
}

impl Mutator for ReplaceInstWithTrap {
    fn name(&self) -> &'static str {
        "replace inst with trap"
    }

    fn mutation_count(&self, func: &Function) -> usize {
        inst_count(func)
    }

    fn mutate(&mut self, mut func: Function) -> Option<(Function, String, ProgressStatus)> {
        next_inst_ret_prev(&func, &mut self.ebb, &mut self.inst).map(|(_prev_ebb, prev_inst)| {
            let status = if func.dfg[prev_inst].opcode() == ir::Opcode::Trap {
                ProgressStatus::Skip
            } else {
                func.dfg.replace(prev_inst).trap(TrapCode::User(0));
                ProgressStatus::Changed
            };
            (
                func,
                format!("Replace inst {} with trap", prev_inst),
                status,
            )
        })
    }
}

/// Try to remove an ebb.
struct RemoveEbb {
    ebb: Ebb,
}

impl RemoveEbb {
    fn new(func: &Function) -> Self {
        Self {
            ebb: func.layout.entry_block().unwrap(),
        }
    }
}

impl Mutator for RemoveEbb {
    fn name(&self) -> &'static str {
        "remove ebb"
    }

    fn mutation_count(&self, func: &Function) -> usize {
        ebb_count(func)
    }

    fn mutate(&mut self, mut func: Function) -> Option<(Function, String, ProgressStatus)> {
        func.layout.next_ebb(self.ebb).map(|next_ebb| {
            self.ebb = next_ebb;
            while let Some(inst) = func.layout.last_inst(self.ebb) {
                func.layout.remove_inst(inst);
            }
            func.layout.remove_ebb(self.ebb);
            (
                func,
                format!("Remove ebb {}", next_ebb),
                ProgressStatus::ExpandedOrShrinked,
            )
        })
    }
}

/// Try to remove unused entities.
struct RemoveUnusedEntities {
    kind: u32,
}

impl RemoveUnusedEntities {
    fn new() -> Self {
        Self { kind: 0 }
    }
}

impl Mutator for RemoveUnusedEntities {
    fn name(&self) -> &'static str {
        "remove unused entities"
    }

    fn mutation_count(&self, _func: &Function) -> usize {
        4
    }

    #[allow(clippy::cognitive_complexity)]
    fn mutate(&mut self, mut func: Function) -> Option<(Function, String, ProgressStatus)> {
        let name = match self.kind {
            0 => {
                let mut ext_func_usage_map = HashMap::new();
                for ebb in func.layout.ebbs() {
                    for inst in func.layout.ebb_insts(ebb) {
                        match func.dfg[inst] {
                            // Add new cases when there are new instruction formats taking a `FuncRef`.
                            InstructionData::Call { func_ref, .. }
                            | InstructionData::FuncAddr { func_ref, .. } => {
                                ext_func_usage_map
                                    .entry(func_ref)
                                    .or_insert_with(Vec::new)
                                    .push(inst);
                            }
                            _ => {}
                        }
                    }
                }

                let mut ext_funcs = PrimaryMap::new();

                for (func_ref, ext_func_data) in func.dfg.ext_funcs.clone().into_iter() {
                    if let Some(func_ref_usage) = ext_func_usage_map.get(&func_ref) {
                        let new_func_ref = ext_funcs.push(ext_func_data.clone());
                        for &inst in func_ref_usage {
                            match func.dfg[inst] {
                                // Keep in sync with the above match.
                                InstructionData::Call {
                                    ref mut func_ref, ..
                                }
                                | InstructionData::FuncAddr {
                                    ref mut func_ref, ..
                                } => {
                                    *func_ref = new_func_ref;
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                func.dfg.ext_funcs = ext_funcs;

                "Remove unused ext funcs"
            }
            1 => {
                #[derive(Copy, Clone)]
                enum SigRefUser {
                    Instruction(Inst),
                    ExtFunc(FuncRef),
                }

                let mut signatures_usage_map = HashMap::new();
                for ebb in func.layout.ebbs() {
                    for inst in func.layout.ebb_insts(ebb) {
                        // Add new cases when there are new instruction formats taking a `SigRef`.
                        if let InstructionData::CallIndirect { sig_ref, .. } = func.dfg[inst] {
                            signatures_usage_map
                                .entry(sig_ref)
                                .or_insert_with(Vec::new)
                                .push(SigRefUser::Instruction(inst));
                        }
                    }
                }
                for (func_ref, ext_func_data) in func.dfg.ext_funcs.iter() {
                    signatures_usage_map
                        .entry(ext_func_data.signature)
                        .or_insert_with(Vec::new)
                        .push(SigRefUser::ExtFunc(func_ref));
                }

                let mut signatures = PrimaryMap::new();

                for (sig_ref, sig_data) in func.dfg.signatures.clone().into_iter() {
                    if let Some(sig_ref_usage) = signatures_usage_map.get(&sig_ref) {
                        let new_sig_ref = signatures.push(sig_data.clone());
                        for &sig_ref_user in sig_ref_usage {
                            match sig_ref_user {
                                SigRefUser::Instruction(inst) => match func.dfg[inst] {
                                    // Keep in sync with the above match.
                                    InstructionData::CallIndirect {
                                        ref mut sig_ref, ..
                                    } => {
                                        *sig_ref = new_sig_ref;
                                    }
                                    _ => unreachable!(),
                                },
                                SigRefUser::ExtFunc(func_ref) => {
                                    func.dfg.ext_funcs[func_ref].signature = new_sig_ref;
                                }
                            }
                        }
                    }
                }

                func.dfg.signatures = signatures;

                "Remove unused signatures"
            }
            2 => {
                let mut stack_slot_usage_map = HashMap::new();
                for ebb in func.layout.ebbs() {
                    for inst in func.layout.ebb_insts(ebb) {
                        match func.dfg[inst] {
                            // Add new cases when there are new instruction formats taking a `StackSlot`.
                            InstructionData::StackLoad { stack_slot, .. }
                            | InstructionData::StackStore { stack_slot, .. } => {
                                stack_slot_usage_map
                                    .entry(stack_slot)
                                    .or_insert_with(Vec::new)
                                    .push(inst);
                            }

                            InstructionData::RegSpill { dst, .. } => {
                                stack_slot_usage_map
                                    .entry(dst)
                                    .or_insert_with(Vec::new)
                                    .push(inst);
                            }
                            InstructionData::RegFill { src, .. } => {
                                stack_slot_usage_map
                                    .entry(src)
                                    .or_insert_with(Vec::new)
                                    .push(inst);
                            }
                            _ => {}
                        }
                    }
                }

                let mut stack_slots = StackSlots::new();

                for (stack_slot, stack_slot_data) in func.stack_slots.clone().iter() {
                    if let Some(stack_slot_usage) = stack_slot_usage_map.get(&stack_slot) {
                        let new_stack_slot = stack_slots.push(stack_slot_data.clone());
                        for &inst in stack_slot_usage {
                            match &mut func.dfg[inst] {
                                // Keep in sync with the above match.
                                InstructionData::StackLoad { stack_slot, .. }
                                | InstructionData::StackStore { stack_slot, .. } => {
                                    *stack_slot = new_stack_slot;
                                }
                                InstructionData::RegSpill { dst, .. } => {
                                    *dst = new_stack_slot;
                                }
                                InstructionData::RegFill { src, .. } => {
                                    *src = new_stack_slot;
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                func.stack_slots = stack_slots;

                "Remove unused stack slots"
            }
            3 => {
                let mut global_value_usage_map = HashMap::new();
                for ebb in func.layout.ebbs() {
                    for inst in func.layout.ebb_insts(ebb) {
                        // Add new cases when there are new instruction formats taking a `GlobalValue`.
                        if let InstructionData::UnaryGlobalValue { global_value, .. } =
                            func.dfg[inst]
                        {
                            global_value_usage_map
                                .entry(global_value)
                                .or_insert_with(Vec::new)
                                .push(inst);
                        }
                    }
                }

                for (_global_value, global_value_data) in func.global_values.iter() {
                    match *global_value_data {
                        GlobalValueData::VMContext | GlobalValueData::Symbol { .. } => {}
                        // These can create cyclic references, which cause complications. Just skip
                        // the global value removal for now.
                        // FIXME Handle them in a better way.
                        GlobalValueData::Load { .. } | GlobalValueData::IAddImm { .. } => {
                            return None
                        }
                    }
                }

                let mut global_values = PrimaryMap::new();

                for (global_value, global_value_data) in func.global_values.clone().into_iter() {
                    if let Some(global_value_usage) = global_value_usage_map.get(&global_value) {
                        let new_global_value = global_values.push(global_value_data.clone());
                        for &inst in global_value_usage {
                            match &mut func.dfg[inst] {
                                // Keep in sync with the above match.
                                InstructionData::UnaryGlobalValue { global_value, .. } => {
                                    *global_value = new_global_value;
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                func.global_values = global_values;

                "Remove unused global values"
            }
            _ => return None,
        };
        self.kind += 1;
        Some((func, name.to_owned(), ProgressStatus::Changed))
    }
}

struct MergeBlocks {
    ebb: Ebb,
    prev_ebb: Option<Ebb>,
}

impl MergeBlocks {
    fn new(func: &Function) -> Self {
        Self {
            ebb: func.layout.entry_block().unwrap(),
            prev_ebb: None,
        }
    }
}

impl Mutator for MergeBlocks {
    fn name(&self) -> &'static str {
        "merge blocks"
    }

    fn mutation_count(&self, func: &Function) -> usize {
        // N ebbs may result in at most N-1 merges.
        ebb_count(func) - 1
    }

    fn mutate(&mut self, mut func: Function) -> Option<(Function, String, ProgressStatus)> {
        let ebb = match func.layout.next_ebb(self.ebb) {
            Some(ebb) => ebb,
            None => return None,
        };

        self.ebb = ebb;

        let mut cfg = ControlFlowGraph::new();
        cfg.compute(&func);

        if cfg.pred_iter(ebb).count() != 1 {
            return Some((
                func,
                format!("did nothing for {}", ebb),
                ProgressStatus::Skip,
            ));
        }

        let pred = cfg.pred_iter(ebb).next().unwrap();

        #[cfg(feature = "basic-blocks")]
        {
            // If the branch instruction that lead us to this block is preceded by another branch
            // instruction, then we have a conditional jump sequence that we should not break by
            // replacing the second instruction by more of them.
            if let Some(pred_pred_inst) = func.layout.prev_inst(pred.inst) {
                if func.dfg[pred_pred_inst].opcode().is_branch() {
                    return Some((
                        func,
                        format!("did nothing for {}", ebb),
                        ProgressStatus::Skip,
                    ));
                }
            }
        }

        assert!(func.dfg.ebb_params(ebb).len() == func.dfg.inst_variable_args(pred.inst).len());

        // If there were any EBB parameters in ebb, then the last instruction in pred will
        // fill these parameters. Make the EBB params aliases of the terminator arguments.
        for (ebb_param, arg) in func
            .dfg
            .detach_ebb_params(ebb)
            .as_slice(&func.dfg.value_lists)
            .iter()
            .cloned()
            .zip(func.dfg.inst_variable_args(pred.inst).iter().cloned())
            .collect::<Vec<_>>()
        {
            if ebb_param != arg {
                func.dfg.change_to_alias(ebb_param, arg);
            }
        }

        // Remove the terminator branch to the current EBB.
        func.layout.remove_inst(pred.inst);

        // Move all the instructions to the predecessor.
        while let Some(inst) = func.layout.first_inst(ebb) {
            func.layout.remove_inst(inst);
            func.layout.append_inst(inst, pred.ebb);
        }

        // Remove the predecessor EBB.
        func.layout.remove_ebb(ebb);

        // Record the previous EBB: if we caused a crash (as signaled by a call to did_crash), then
        // we'll start back to this EBB.
        self.prev_ebb = Some(pred.ebb);

        Some((
            func,
            format!("merged {} and {}", pred.ebb, ebb),
            ProgressStatus::ExpandedOrShrinked,
        ))
    }

    fn did_crash(&mut self) {
        self.ebb = self.prev_ebb.unwrap();
    }
}

fn next_inst_ret_prev(func: &Function, ebb: &mut Ebb, inst: &mut Inst) -> Option<(Ebb, Inst)> {
    let prev = (*ebb, *inst);
    if let Some(next_inst) = func.layout.next_inst(*inst) {
        *inst = next_inst;
        return Some(prev);
    }
    if let Some(next_ebb) = func.layout.next_ebb(*ebb) {
        *ebb = next_ebb;
        *inst = func.layout.first_inst(*ebb).expect("no inst");
        return Some(prev);
    }
    None
}

fn ebb_count(func: &Function) -> usize {
    func.layout.ebbs().count()
}

fn inst_count(func: &Function) -> usize {
    func.layout
        .ebbs()
        .map(|ebb| func.layout.ebb_insts(ebb).count())
        .sum()
}

fn resolve_aliases(func: &mut Function) {
    for ebb in func.layout.ebbs() {
        for inst in func.layout.ebb_insts(ebb) {
            func.dfg.resolve_aliases_in_arguments(inst);
        }
    }
}

fn reduce(
    isa: &dyn TargetIsa,
    mut func: Function,
    verbose: bool,
) -> Result<(Function, String), String> {
    let mut context = CrashCheckContext::new(isa);

    match context.check_for_crash(&func) {
        CheckResult::Succeed => {
            return Err(
                "Given function compiled successfully or gave a verifier error.".to_string(),
            );
        }
        CheckResult::Crash(_) => {}
    }

    resolve_aliases(&mut func);

    let progress_bar = ProgressBar::with_draw_target(0, ProgressDrawTarget::stdout());
    progress_bar.set_style(
        ProgressStyle::default_bar().template("{bar:60} {prefix:40} {pos:>4}/{len:>4} {msg}"),
    );

    for pass_idx in 0..100 {
        let mut should_keep_reducing = false;
        let mut phase = 0;

        loop {
            let mut mutator: Box<dyn Mutator> = match phase {
                0 => Box::new(RemoveInst::new(&func)),
                1 => Box::new(ReplaceInstWithConst::new(&func)),
                2 => Box::new(ReplaceInstWithTrap::new(&func)),
                3 => Box::new(RemoveEbb::new(&func)),
                4 => Box::new(RemoveUnusedEntities::new()),
                5 => Box::new(MergeBlocks::new(&func)),
                _ => break,
            };

            progress_bar.set_prefix(&format!("pass {} phase {}", pass_idx, mutator.name()));
            progress_bar.set_length(mutator.mutation_count(&func) as u64);

            // Reset progress bar.
            progress_bar.set_position(0);
            progress_bar.set_draw_delta(0);

            for _ in 0..10000 {
                progress_bar.inc(1);

                let (mutated_func, msg, mutation_kind) = match mutator.mutate(func.clone()) {
                    Some(res) => res,
                    None => {
                        break;
                    }
                };

                if let ProgressStatus::Skip = mutation_kind {
                    // The mutator didn't change anything, but we want to try more mutator
                    // iterations.
                    continue;
                }

                progress_bar.set_message(&msg);

                match context.check_for_crash(&mutated_func) {
                    CheckResult::Succeed => {
                        // Mutating didn't hit the problem anymore, discard changes.
                        continue;
                    }
                    CheckResult::Crash(_) => {
                        // Panic remained while mutating, make changes definitive.
                        func = mutated_func;

                        // Notify the mutator that the mutation was successful.
                        mutator.did_crash();

                        let verb = match mutation_kind {
                            ProgressStatus::ExpandedOrShrinked => {
                                should_keep_reducing = true;
                                "shrink"
                            }
                            ProgressStatus::Changed => "changed",
                            ProgressStatus::Skip => unreachable!(),
                        };
                        if verbose {
                            progress_bar.println(format!("{}: {}", msg, verb));
                        }
                    }
                }
            }

            phase += 1;
        }

        progress_bar.println(format!(
            "After pass {}, remaining insts/ebbs: {}/{} ({})",
            pass_idx,
            inst_count(&func),
            ebb_count(&func),
            if should_keep_reducing {
                "will keep reducing"
            } else {
                "stop reducing"
            }
        ));

        if !should_keep_reducing {
            // No new shrinking opportunities have been found this pass. This means none will ever
            // be found. Skip the rest of the passes over the function.
            break;
        }
    }

    progress_bar.finish();

    let crash_msg = match context.check_for_crash(&func) {
        CheckResult::Succeed => unreachable!("Used to crash, but doesn't anymore???"),
        CheckResult::Crash(crash_msg) => crash_msg,
    };

    Ok((func, crash_msg))
}

struct CrashCheckContext<'a> {
    /// Cached `Context`, to prevent repeated allocation.
    context: Context,

    /// Cached code memory, to prevent repeated allocation.
    code_memory: Vec<u8>,

    /// The target isa to compile for.
    isa: &'a dyn TargetIsa,
}

fn get_panic_string(panic: Box<dyn std::any::Any>) -> String {
    let panic = match panic.downcast::<&'static str>() {
        Ok(panic_msg) => {
            return panic_msg.to_string();
        }
        Err(panic) => panic,
    };
    match panic.downcast::<String>() {
        Ok(panic_msg) => *panic_msg,
        Err(_) => "Box<Any>".to_string(),
    }
}

enum CheckResult {
    /// The function compiled fine, or the verifier noticed an error.
    Succeed,

    /// The compilation of the function panicked.
    Crash(String),
}

impl<'a> CrashCheckContext<'a> {
    fn new(isa: &'a dyn TargetIsa) -> Self {
        CrashCheckContext {
            context: Context::new(),
            code_memory: Vec::new(),
            isa,
        }
    }

    #[cfg_attr(test, allow(unreachable_code))]
    fn check_for_crash(&mut self, func: &Function) -> CheckResult {
        self.context.clear();
        self.code_memory.clear();

        self.context.func = func.clone();

        use std::io::Write;
        std::io::stdout().flush().unwrap(); // Flush stdout to sync with panic messages on stderr

        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cranelift_codegen::verifier::verify_function(&func, self.isa).err()
        })) {
            Ok(Some(_)) => return CheckResult::Succeed,
            Ok(None) => {}
            // The verifier panicked. Compiling it will probably give the same panic.
            // We treat it as succeeding to make it possible to reduce for the actual error.
            // FIXME prevent verifier panic on removing ebb0.
            Err(_) => return CheckResult::Succeed,
        }

        #[cfg(test)]
        {
            // For testing purposes we emulate a panic caused by the existence of
            // a `call` instruction.
            let contains_call = func.layout.ebbs().any(|ebb| {
                func.layout.ebb_insts(ebb).any(|inst| match func.dfg[inst] {
                    InstructionData::Call { .. } => true,
                    _ => false,
                })
            });
            if contains_call {
                return CheckResult::Crash("test crash".to_string());
            } else {
                return CheckResult::Succeed;
            }
        }

        let old_panic_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence panics

        let res = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut relocs = PrintRelocs::new(false);
            let mut traps = PrintTraps::new(false);
            let mut stackmaps = PrintStackmaps::new(false);

            let _ = self.context.compile_and_emit(
                self.isa,
                &mut self.code_memory,
                &mut relocs,
                &mut traps,
                &mut stackmaps,
            );
        })) {
            Ok(()) => CheckResult::Succeed,
            Err(err) => CheckResult::Crash(get_panic_string(err)),
        };

        std::panic::set_hook(old_panic_hook);

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_reader::ParseOptions;

    #[test]
    fn test_reduce() {
        const TEST: &str = include_str!("../tests/bugpoint_test.clif");
        const EXPECTED: &str = include_str!("../tests/bugpoint_test_expected.clif");

        let test_file = parse_test(TEST, ParseOptions::default()).unwrap();

        // If we have an isa from the command-line, use that. Otherwise if the
        // file contains a unique isa, use that.
        let isa = test_file.isa_spec.unique_isa().expect("Unknown isa");

        for (func, _) in test_file.functions {
            let (reduced_func, crash_msg) =
                reduce(isa, func, false).expect("Couldn't reduce test case");
            assert_eq!(crash_msg, "test crash");

            let (func_reduced_twice, crash_msg) =
                reduce(isa, reduced_func.clone(), false).expect("Couldn't re-reduce test case");
            assert_eq!(crash_msg, "test crash");

            assert_eq!(
                ebb_count(&func_reduced_twice),
                ebb_count(&reduced_func),
                "reduction wasn't maximal for ebbs"
            );
            assert_eq!(
                inst_count(&func_reduced_twice),
                inst_count(&reduced_func),
                "reduction wasn't maximal for insts"
            );

            assert_eq!(format!("{}", reduced_func), EXPECTED.replace("\r\n", "\n"));
        }
    }
}
