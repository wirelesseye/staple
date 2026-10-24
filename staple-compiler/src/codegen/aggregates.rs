//! Moves large aggregate values out of LLVM registers and into memory.
//!
//! Emission treats every product as an SSA value. That is fine for small
//! products, but LLVM's instruction selector splits an aggregate `load`,
//! `store`, argument or return value into one operation per element, so a
//! `(I32 ^ 100000)` costs gigabytes of compiler memory or crashes it. After
//! emission this pass gives each large aggregate value a *home*: a pointer to
//! memory that holds the value and is written only by the instruction that
//! defines it. Loads, stores and `select` become `memcpy`, `extractvalue`
//! becomes address arithmetic, `insertvalue` writes in place, and functions
//! take and return large values through pointers.
//!
//! Only functions with the Staple calling convention change signature. Native
//! externs keep their C signatures, and any use the pass does not understand
//! reloads the value from its home, so the module stays valid.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char, c_uint};

use inkwell::llvm_sys::LLVMOpcode;
use inkwell::llvm_sys::LLVMTypeKind;
use inkwell::llvm_sys::core::*;
use inkwell::llvm_sys::prelude::*;
use inkwell::llvm_sys::target::{
    LLVMABIAlignmentOfType, LLVMGetModuleDataLayout, LLVMStoreSizeOfType, LLVMTargetDataRef,
};
use inkwell::module::Module;

/// Aggregates whose store size exceeds this many bytes live in memory.
pub(crate) const LARGE_AGGREGATE_BYTES: u64 = 256;

/// Rewrites every function in `module` so that no aggregate larger than
/// [`LARGE_AGGREGATE_BYTES`] is held in a register. `native` names the
/// functions that use the C ABI and must keep their signatures.
pub(crate) fn legalize_large_aggregates(module: &Module<'_>, native: &HashSet<String>) {
    // SAFETY: every reference comes from `module`, which outlives the pass, and
    // each rewrite keeps the module well typed before the next one runs.
    unsafe { Legalizer::new(module.as_mut_ptr(), native).run() }
}

struct Legalizer<'a> {
    module: LLVMModuleRef,
    context: LLVMContextRef,
    builder: LLVMBuilderRef,
    data: LLVMTargetDataRef,
    native: &'a HashSet<String>,
    /// Functions whose signature passes large values through pointers, with
    /// the index of their trailing result pointer when the result is large.
    rewritten: HashMap<LLVMValueRef, Option<c_uint>>,
    /// Homes of large parameters, from a signature rewrite to the body rewrite
    /// of the same function. Keys are the old function's arguments.
    parameter_homes: HashMap<LLVMValueRef, Vec<(LLVMValueRef, LLVMValueRef)>>,
    /// Old functions, emptied and redirected, deleted once every body has
    /// been rewritten so their arguments stay valid as map keys until then.
    retired: Vec<LLVMValueRef>,
}

/// Runs of at least this many identical parameters travel as one pointer to
/// an array the caller fills, so a function over `(I32 ^ 100000)` does not
/// have 100000 LLVM parameters.
const PACKED_PARAMETER_RUN: usize = 16;

/// How a function type's parameters change.
struct SignaturePlan {
    new_type: LLVMTypeRef,
    parameters: Vec<NewParameter>,
    large_result: bool,
}

/// One parameter of the rewritten signature.
enum NewParameter {
    /// Old parameter `old`, passed through its home when `home` is set.
    Direct { old: usize, home: bool },
    /// Old parameters `start..start + length`, all of type `element`, read
    /// from an array; `homes` marks large values passed as their homes.
    Packed {
        start: usize,
        length: usize,
        element: LLVMTypeRef,
        homes: bool,
    },
}

/// Per-function rewrite state.
#[derive(Default)]
struct Homes {
    /// The memory holding each large value.
    homes: HashMap<LLVMValueRef, LLVMValueRef>,
    /// Instructions replaced by memory operations, erased at the end.
    dead: Vec<LLVMValueRef>,
    /// Inserts that write into the home of the value they extend.
    in_place: HashSet<LLVMValueRef>,
    /// Values whose remaining uses must reload them from their homes: the
    /// dead instructions plus the old large parameters.
    replaced: Vec<LLVMValueRef>,
    /// The function's result pointer when it returns a large value.
    result: Option<LLVMValueRef>,
}

impl<'a> Legalizer<'a> {
    unsafe fn new(module: LLVMModuleRef, native: &'a HashSet<String>) -> Self {
        unsafe {
            let context = LLVMGetModuleContext(module);
            Self {
                module,
                context,
                builder: LLVMCreateBuilderInContext(context),
                data: LLVMGetModuleDataLayout(module),
                native,
                rewritten: HashMap::new(),
                parameter_homes: HashMap::new(),
                retired: Vec::new(),
            }
        }
    }

    unsafe fn run(mut self) {
        unsafe {
            let functions = self.functions();
            for function in functions {
                self.rewrite_signature(function);
            }
            for function in self.functions() {
                if LLVMIsDeclaration(function) == 0 {
                    self.rewrite_body(function);
                }
            }
            for function in std::mem::take(&mut self.retired) {
                LLVMDeleteFunction(function);
            }
            LLVMDisposeBuilder(self.builder);
        }
    }

    unsafe fn functions(&self) -> Vec<LLVMValueRef> {
        unsafe {
            let mut functions = Vec::new();
            let mut function = LLVMGetFirstFunction(self.module);
            while !function.is_null() {
                functions.push(function);
                function = LLVMGetNextFunction(function);
            }
            functions
        }
    }

    unsafe fn is_large(&self, ty: LLVMTypeRef) -> bool {
        unsafe {
            matches!(
                LLVMGetTypeKind(ty),
                LLVMTypeKind::LLVMStructTypeKind | LLVMTypeKind::LLVMArrayTypeKind
            ) && LLVMTypeIsSized(ty) != 0
                && LLVMStoreSizeOfType(self.data, ty) > LARGE_AGGREGATE_BYTES
        }
    }

    unsafe fn name(&self, value: LLVMValueRef) -> String {
        unsafe {
            let mut length = 0;
            let name = LLVMGetValueName2(value, &mut length);
            if name.is_null() {
                return String::new();
            }
            String::from_utf8_lossy(std::slice::from_raw_parts(name.cast::<u8>(), length))
                .into_owned()
        }
    }

    /// Whether a function keeps its signature: an intrinsic or a C extern.
    unsafe fn keeps_abi(&self, function: LLVMValueRef) -> bool {
        unsafe { LLVMGetIntrinsicID(function) != 0 || self.native.contains(&self.name(function)) }
    }

    /// How a Staple-convention function type changes, or `None` when it has
    /// no large parameter or result and no long run of identical parameters.
    unsafe fn signature_plan(&self, ty: LLVMTypeRef) -> Option<SignaturePlan> {
        unsafe {
            if LLVMIsFunctionVarArg(ty) != 0 {
                return None;
            }
            let result = LLVMGetReturnType(ty);
            let count = LLVMCountParamTypes(ty) as usize;
            let mut old = vec![std::ptr::null_mut(); count];
            LLVMGetParamTypes(ty, old.as_mut_ptr());
            let pointer = LLVMPointerTypeInContext(self.context, 0);
            let large = old.iter().map(|&ty| self.is_large(ty)).collect::<Vec<_>>();
            let mapped = old
                .iter()
                .zip(&large)
                .map(|(&ty, &large)| if large { pointer } else { ty })
                .collect::<Vec<_>>();
            let large_result = self.is_large(result);
            let mut parameters = Vec::new();
            let mut new_types = Vec::new();
            let mut index = 0;
            while index < count {
                let mut end = index + 1;
                while end < count && mapped[end] == mapped[index] && large[end] == large[index] {
                    end += 1;
                }
                if end - index >= PACKED_PARAMETER_RUN {
                    parameters.push(NewParameter::Packed {
                        start: index,
                        length: end - index,
                        element: mapped[index],
                        homes: large[index],
                    });
                    new_types.push(pointer);
                } else {
                    for position in index..end {
                        parameters.push(NewParameter::Direct {
                            old: position,
                            home: large[position],
                        });
                        new_types.push(mapped[position]);
                    }
                }
                index = end;
            }
            let packed = parameters
                .iter()
                .any(|parameter| matches!(parameter, NewParameter::Packed { .. }));
            if !large_result && !packed && !large.iter().any(|&large| large) {
                return None;
            }
            let new_result = if large_result {
                new_types.push(pointer);
                LLVMVoidTypeInContext(self.context)
            } else {
                result
            };
            Some(SignaturePlan {
                new_type: LLVMFunctionType(
                    new_result,
                    new_types.as_mut_ptr(),
                    new_types.len() as c_uint,
                    0,
                ),
                parameters,
                large_result,
            })
        }
    }

    unsafe fn rewrite_signature(&mut self, function: LLVMValueRef) {
        unsafe {
            if self.keeps_abi(function) {
                return;
            }
            let old_type = LLVMGlobalGetValueType(function);
            let Some(plan) = self.signature_plan(old_type) else {
                return;
            };
            let name = self.name(function);
            LLVMSetValueName2(function, c"".as_ptr(), 0);
            let replacement = LLVMAddFunction(self.module, c"".as_ptr(), plan.new_type);
            LLVMSetValueName2(replacement, name.as_ptr().cast::<c_char>(), name.len());
            LLVMSetLinkage(replacement, LLVMGetLinkage(function));
            LLVMSetVisibility(replacement, LLVMGetVisibility(function));
            LLVMSetFunctionCallConv(replacement, LLVMGetFunctionCallConv(function));
            LLVMSetUnnamedAddress(replacement, LLVMGetUnnamedAddress(function));
            let section = LLVMGetSection(function);
            if !section.is_null() && !CStr::from_ptr(section).is_empty() {
                LLVMSetSection(replacement, section);
            }
            copy_function_attributes(function, replacement);

            // Move the body, then bind each old parameter to its new source.
            // A large parameter's home is the pointer it now arrives as.
            let has_body = LLVMIsDeclaration(function) == 0;
            if has_body {
                while let block = LLVMGetFirstBasicBlock(function)
                    && !block.is_null()
                {
                    LLVMRemoveBasicBlockFromParent(block);
                    LLVMAppendExistingBasicBlock(replacement, block);
                }
                let entry = LLVMGetEntryBasicBlock(replacement);
                LLVMPositionBuilderBefore(self.builder, LLVMGetFirstInstruction(entry));
            }
            let mut homes = Vec::new();
            let mut bind = |old: LLVMValueRef, new: LLVMValueRef, home: bool| {
                if home {
                    homes.push((old, new));
                } else {
                    LLVMReplaceAllUsesWith(old, new);
                }
            };
            for (new_index, parameter) in plan.parameters.iter().enumerate() {
                let new = LLVMGetParam(replacement, new_index as c_uint);
                match *parameter {
                    NewParameter::Direct { old, home } => {
                        let old = LLVMGetParam(function, old as c_uint);
                        let mut length = 0;
                        let parameter_name = LLVMGetValueName2(old, &mut length);
                        LLVMSetValueName2(new, parameter_name, length);
                        bind(old, new, home);
                    }
                    NewParameter::Packed {
                        start,
                        length,
                        element,
                        homes: large,
                    } => {
                        LLVMSetValueName2(new, c"packed".as_ptr(), 6);
                        if !has_body {
                            continue;
                        }
                        let array = LLVMArrayType2(element, length as u64);
                        for offset in 0..length {
                            let old = LLVMGetParam(function, (start + offset) as c_uint);
                            let pointer = self.array_element(array, new, offset);
                            let value = LLVMBuildLoad2(
                                self.builder,
                                element,
                                pointer,
                                c"packed.parameter".as_ptr(),
                            );
                            LLVMSetAlignment(value, self.align_of(element));
                            bind(old, value, large);
                        }
                    }
                }
            }
            let result_index = plan.large_result.then(|| LLVMCountParams(replacement) - 1);
            if let Some(index) = result_index {
                LLVMSetValueName2(LLVMGetParam(replacement, index), c"result".as_ptr(), 6);
            }
            // The large parameters are rewritten with the body; until then
            // their uses refer to the old function's arguments.
            self.parameter_homes.insert(replacement, homes);
            LLVMReplaceAllUsesWith(function, replacement);
            self.retired.push(function);
            self.rewritten.insert(replacement, result_index);
        }
    }

    unsafe fn array_element(
        &self,
        array: LLVMTypeRef,
        base: LLVMValueRef,
        offset: usize,
    ) -> LLVMValueRef {
        unsafe {
            let i64_type = LLVMInt64TypeInContext(self.context);
            let mut indices = [
                LLVMConstInt(i64_type, 0, 0),
                LLVMConstInt(i64_type, offset as u64, 0),
            ];
            LLVMBuildInBoundsGEP2(
                self.builder,
                array,
                base,
                indices.as_mut_ptr(),
                2,
                c"packed.element".as_ptr(),
            )
        }
    }

    unsafe fn rewrite_body(&mut self, function: LLVMValueRef) {
        unsafe {
            let mut state = Homes::default();
            let parameters = self.parameter_homes.remove(&function).unwrap_or_default();
            state.homes.extend(parameters.iter().copied());
            state
                .replaced
                .extend(parameters.iter().map(|(old, _)| *old));
            if let Some(Some(index)) = self.rewritten.get(&function) {
                state.result = Some(LLVMGetParam(function, *index));
            }
            let instructions = instructions(function);
            if parameters.is_empty()
                && state.result.is_none()
                && !instructions
                    .iter()
                    .any(|&instruction| self.touches_large(instruction))
            {
                return;
            }
            // Every home exists before any use is rewritten, so the order in
            // which blocks are visited does not matter.
            for &instruction in &instructions {
                self.assign_home(function, instruction, &mut state);
            }
            for &instruction in &instructions {
                self.rewrite_instruction(function, instruction, &mut state);
            }
            self.rewrite_phis(function, &instructions, &mut state);
            self.erase(&mut state);
        }
    }

    /// Chooses the memory that holds a large value defined by `instruction`.
    unsafe fn assign_home(
        &self,
        function: LLVMValueRef,
        instruction: LLVMValueRef,
        state: &mut Homes,
    ) -> Option<LLVMValueRef> {
        unsafe {
            if let Some(home) = state.homes.get(&instruction) {
                return Some(*home);
            }
            let ty = LLVMTypeOf(instruction);
            if LLVMIsAInstruction(instruction).is_null() || !self.is_large(ty) {
                return None;
            }
            let home = match LLVMGetInstructionOpcode(instruction) {
                LLVMOpcode::LLVMLoad => {
                    let source = LLVMGetOperand(instruction, 0);
                    if final_load_of_slot(instruction, source) {
                        // The slot is never written again, so it already is
                        // an immutable home and the copy can be skipped.
                        state.in_place.insert(instruction);
                        source
                    } else {
                        self.entry_alloca(function, ty)
                    }
                }
                LLVMOpcode::LLVMSelect | LLVMOpcode::LLVMPHI => self.entry_alloca(function, ty),
                LLVMOpcode::LLVMCall if self.call_is_rewritten(instruction) => {
                    self.entry_alloca(function, ty)
                }
                LLVMOpcode::LLVMInsertValue => {
                    // A chain of single-use inserts builds one value in place.
                    let aggregate = LLVMGetOperand(instruction, 0);
                    let chained = !LLVMIsAInstruction(aggregate).is_null()
                        && LLVMGetInstructionOpcode(aggregate) == LLVMOpcode::LLVMInsertValue
                        && has_single_use(aggregate);
                    match chained
                        .then(|| self.assign_home(function, aggregate, state))
                        .flatten()
                    {
                        Some(home) => {
                            state.in_place.insert(instruction);
                            home
                        }
                        None => self.entry_alloca(function, ty),
                    }
                }
                LLVMOpcode::LLVMExtractValue => {
                    let aggregate = LLVMGetOperand(instruction, 0);
                    let base = match self.assign_home(function, aggregate, state) {
                        Some(base) => base,
                        None => self.home_of(function, aggregate, state),
                    };
                    LLVMPositionBuilderBefore(self.builder, instruction);
                    // The parent's home is never rewritten while this value is
                    // live, so the element can share it.
                    self.element_pointer(LLVMTypeOf(aggregate), base, instruction)
                }
                _ => return None,
            };
            state.homes.insert(instruction, home);
            Some(home)
        }
    }

    unsafe fn call_is_rewritten(&self, instruction: LLVMValueRef) -> bool {
        unsafe {
            let direct = LLVMIsAFunction(LLVMGetCalledValue(instruction));
            (direct.is_null() || !self.keeps_abi(direct))
                && self
                    .signature_plan(LLVMGetCalledFunctionType(instruction))
                    .is_some()
        }
    }

    unsafe fn touches_large(&self, instruction: LLVMValueRef) -> bool {
        unsafe {
            if self.is_large(LLVMTypeOf(instruction)) {
                return true;
            }
            for index in 0..LLVMGetNumOperands(instruction) {
                let operand = LLVMGetOperand(instruction, index as c_uint);
                if !operand.is_null() && self.is_large(LLVMTypeOf(operand)) {
                    return true;
                }
            }
            if LLVMGetInstructionOpcode(instruction) == LLVMOpcode::LLVMCall {
                return self.call_is_rewritten(instruction);
            }
            false
        }
    }

    unsafe fn entry_alloca(&self, function: LLVMValueRef, ty: LLVMTypeRef) -> LLVMValueRef {
        unsafe {
            let entry = LLVMGetEntryBasicBlock(function);
            let first = LLVMGetFirstInstruction(entry);
            if first.is_null() {
                LLVMPositionBuilderAtEnd(self.builder, entry);
            } else {
                LLVMPositionBuilderBefore(self.builder, first);
            }
            let slot = LLVMBuildAlloca(self.builder, ty, c"aggregate.home".as_ptr());
            LLVMSetAlignment(slot, LLVMABIAlignmentOfType(self.data, ty));
            slot
        }
    }

    unsafe fn size_of(&self, ty: LLVMTypeRef) -> LLVMValueRef {
        unsafe {
            LLVMConstInt(
                LLVMInt64TypeInContext(self.context),
                LLVMStoreSizeOfType(self.data, ty),
                0,
            )
        }
    }

    unsafe fn align_of(&self, ty: LLVMTypeRef) -> c_uint {
        unsafe { LLVMABIAlignmentOfType(self.data, ty) }
    }

    /// Copies a large value of type `ty` between two addresses at the
    /// builder's position.
    unsafe fn copy(
        &self,
        ty: LLVMTypeRef,
        destination: LLVMValueRef,
        destination_align: c_uint,
        source: LLVMValueRef,
        source_align: c_uint,
    ) {
        unsafe {
            LLVMBuildMemMove(
                self.builder,
                destination,
                destination_align,
                source,
                source_align,
                self.size_of(ty),
            );
        }
    }

    /// The address holding a large operand. Constants get a home of their own
    /// in the entry block.
    unsafe fn home_of(
        &self,
        function: LLVMValueRef,
        value: LLVMValueRef,
        state: &mut Homes,
    ) -> LLVMValueRef {
        unsafe {
            if let Some(home) = state.homes.get(&value) {
                return *home;
            }
            let ty = LLVMTypeOf(value);
            let home = self.entry_alloca(function, ty);
            let after = LLVMGetNextInstruction(home);
            LLVMPositionBuilderBefore(self.builder, after);
            if LLVMIsAConstant(value).is_null() {
                // A value the pass does not own, such as the result of an
                // instruction it left alone: spill it where it is defined.
                let instruction = LLVMIsAInstruction(value);
                if !instruction.is_null() {
                    let next = LLVMGetNextInstruction(instruction);
                    LLVMPositionBuilderBefore(self.builder, next);
                }
                let store = LLVMBuildStore(self.builder, value, home);
                LLVMSetAlignment(store, self.align_of(ty));
            } else if LLVMIsUndef(value) == 0 && LLVMIsPoison(value) == 0 {
                if LLVMIsNull(value) != 0 {
                    LLVMBuildMemSet(
                        self.builder,
                        home,
                        LLVMConstInt(LLVMInt8TypeInContext(self.context), 0, 0),
                        self.size_of(ty),
                        self.align_of(ty),
                    );
                } else {
                    let store = LLVMBuildStore(self.builder, value, home);
                    LLVMSetAlignment(store, self.align_of(ty));
                }
            }
            state.homes.insert(value, home);
            home
        }
    }

    unsafe fn element_pointer(
        &self,
        aggregate_type: LLVMTypeRef,
        base: LLVMValueRef,
        instruction: LLVMValueRef,
    ) -> LLVMValueRef {
        unsafe {
            let count = LLVMGetNumIndices(instruction) as usize;
            let indices = std::slice::from_raw_parts(LLVMGetIndices(instruction), count);
            let i32_type = LLVMInt32TypeInContext(self.context);
            let mut operands = Vec::with_capacity(count + 1);
            operands.push(LLVMConstInt(i32_type, 0, 0));
            operands.extend(
                indices
                    .iter()
                    .map(|index| LLVMConstInt(i32_type, u64::from(*index), 0)),
            );
            LLVMBuildInBoundsGEP2(
                self.builder,
                aggregate_type,
                base,
                operands.as_mut_ptr(),
                operands.len() as c_uint,
                c"aggregate.element".as_ptr(),
            )
        }
    }

    unsafe fn rewrite_instruction(
        &self,
        function: LLVMValueRef,
        instruction: LLVMValueRef,
        state: &mut Homes,
    ) {
        unsafe {
            let ty = LLVMTypeOf(instruction);
            match LLVMGetInstructionOpcode(instruction) {
                LLVMOpcode::LLVMLoad if self.is_large(ty) => {
                    let home = state.homes[&instruction];
                    if state.in_place.contains(&instruction) {
                        state.dead.push(instruction);
                        return;
                    }
                    LLVMPositionBuilderBefore(self.builder, instruction);
                    let source = LLVMGetOperand(instruction, 0);
                    self.copy(
                        ty,
                        home,
                        self.align_of(ty),
                        source,
                        LLVMGetAlignment(instruction).max(1),
                    );
                    state.dead.push(instruction);
                }
                LLVMOpcode::LLVMStore => {
                    let value = LLVMGetOperand(instruction, 0);
                    let value_type = LLVMTypeOf(value);
                    if !self.is_large(value_type) {
                        return;
                    }
                    let source = self.home_of(function, value, state);
                    LLVMPositionBuilderBefore(self.builder, instruction);
                    self.copy(
                        value_type,
                        LLVMGetOperand(instruction, 1),
                        LLVMGetAlignment(instruction).max(1),
                        source,
                        self.align_of(value_type),
                    );
                    state.dead.push(instruction);
                }
                LLVMOpcode::LLVMExtractValue => {
                    let aggregate = LLVMGetOperand(instruction, 0);
                    let aggregate_type = LLVMTypeOf(aggregate);
                    if !self.is_large(aggregate_type) {
                        return;
                    }
                    if !self.is_large(ty) {
                        let base = self.home_of(function, aggregate, state);
                        LLVMPositionBuilderBefore(self.builder, instruction);
                        let pointer = self.element_pointer(aggregate_type, base, instruction);
                        let load =
                            LLVMBuildLoad2(self.builder, ty, pointer, c"aggregate.load".as_ptr());
                        LLVMSetAlignment(load, self.align_of(ty));
                        LLVMReplaceAllUsesWith(instruction, load);
                    }
                    state.dead.push(instruction);
                }
                LLVMOpcode::LLVMInsertValue if self.is_large(ty) => {
                    let aggregate = LLVMGetOperand(instruction, 0);
                    let element = LLVMGetOperand(instruction, 1);
                    let home = state.homes[&instruction];
                    if !state.in_place.contains(&instruction) {
                        let constant = !LLVMIsAConstant(aggregate).is_null();
                        if constant && LLVMIsNull(aggregate) != 0 {
                            LLVMPositionBuilderBefore(self.builder, instruction);
                            LLVMBuildMemSet(
                                self.builder,
                                home,
                                LLVMConstInt(LLVMInt8TypeInContext(self.context), 0, 0),
                                self.size_of(ty),
                                self.align_of(ty),
                            );
                        } else if !(constant
                            && (LLVMIsUndef(aggregate) != 0 || LLVMIsPoison(aggregate) != 0))
                        {
                            let source = self.home_of(function, aggregate, state);
                            LLVMPositionBuilderBefore(self.builder, instruction);
                            self.copy(ty, home, self.align_of(ty), source, self.align_of(ty));
                        }
                    }
                    let element_type = LLVMTypeOf(element);
                    let element_home = self
                        .is_large(element_type)
                        .then(|| self.home_of(function, element, state));
                    LLVMPositionBuilderBefore(self.builder, instruction);
                    let pointer = self.element_pointer(ty, home, instruction);
                    match element_home {
                        Some(source) => self.copy(
                            element_type,
                            pointer,
                            self.align_of(element_type),
                            source,
                            self.align_of(element_type),
                        ),
                        None => {
                            let store = LLVMBuildStore(self.builder, element, pointer);
                            LLVMSetAlignment(store, self.align_of(element_type));
                        }
                    }
                    state.dead.push(instruction);
                }
                LLVMOpcode::LLVMSelect if self.is_large(ty) => {
                    let when_true = self.home_of(function, LLVMGetOperand(instruction, 1), state);
                    let when_false = self.home_of(function, LLVMGetOperand(instruction, 2), state);
                    let home = state.homes[&instruction];
                    LLVMPositionBuilderBefore(self.builder, instruction);
                    let chosen = LLVMBuildSelect(
                        self.builder,
                        LLVMGetOperand(instruction, 0),
                        when_true,
                        when_false,
                        c"aggregate.select".as_ptr(),
                    );
                    self.copy(ty, home, self.align_of(ty), chosen, self.align_of(ty));
                    state.dead.push(instruction);
                }
                LLVMOpcode::LLVMRet => {
                    let Some(result) = state.result else {
                        return;
                    };
                    let value = LLVMGetOperand(instruction, 0);
                    let value_type = LLVMTypeOf(value);
                    let source = self.home_of(function, value, state);
                    LLVMPositionBuilderBefore(self.builder, instruction);
                    self.copy(
                        value_type,
                        result,
                        self.align_of(value_type),
                        source,
                        self.align_of(value_type),
                    );
                    LLVMBuildRetVoid(self.builder);
                    state.dead.push(instruction);
                }
                LLVMOpcode::LLVMCall => self.rewrite_call(function, instruction, state),
                _ => {}
            }
        }
    }

    unsafe fn rewrite_call(
        &self,
        function: LLVMValueRef,
        instruction: LLVMValueRef,
        state: &mut Homes,
    ) {
        unsafe {
            if !self.call_is_rewritten(instruction) {
                return;
            }
            let callee = LLVMGetCalledValue(instruction);
            let plan = self
                .signature_plan(LLVMGetCalledFunctionType(instruction))
                .expect("a rewritten call has a plan");
            let mut argument = |index: usize, state: &mut Homes| {
                let value = LLVMGetOperand(instruction, index as c_uint);
                if self.is_large(LLVMTypeOf(value)) {
                    self.home_of(function, value, state)
                } else {
                    value
                }
            };
            let mut arguments = Vec::with_capacity(plan.parameters.len() + 1);
            for parameter in &plan.parameters {
                match *parameter {
                    NewParameter::Direct { old, .. } => arguments.push(argument(old, state)),
                    NewParameter::Packed {
                        start,
                        length,
                        element,
                        ..
                    } => {
                        let array = LLVMArrayType2(element, length as u64);
                        let storage = self.entry_alloca(function, array);
                        for offset in 0..length {
                            let value = argument(start + offset, state);
                            LLVMPositionBuilderBefore(self.builder, instruction);
                            let pointer = self.array_element(array, storage, offset);
                            let store = LLVMBuildStore(self.builder, value, pointer);
                            LLVMSetAlignment(store, self.align_of(element));
                        }
                        arguments.push(storage);
                    }
                }
            }
            let result_type = LLVMTypeOf(instruction);
            let result_home = plan.large_result.then(|| state.homes[&instruction]);
            arguments.extend(result_home);
            LLVMPositionBuilderBefore(self.builder, instruction);
            let call = LLVMBuildCall2(
                self.builder,
                plan.new_type,
                callee,
                arguments.as_mut_ptr(),
                arguments.len() as c_uint,
                c"".as_ptr(),
            );
            LLVMSetInstructionCallConv(call, LLVMGetInstructionCallConv(instruction));
            match result_home {
                Some(_) => {}
                None => {
                    if LLVMGetTypeKind(result_type) != LLVMTypeKind::LLVMVoidTypeKind {
                        let mut length = 0;
                        let name = LLVMGetValueName2(instruction, &mut length);
                        LLVMSetValueName2(call, name, length);
                    }
                    LLVMReplaceAllUsesWith(instruction, call);
                }
            }
            state.dead.push(instruction);
        }
    }

    /// Replaces each large phi with a phi of home addresses and copies the
    /// chosen value into the phi's own home at the top of its block. Copies
    /// go through a temporary when a block has several large phis, so phis
    /// that read each other see their old values.
    unsafe fn rewrite_phis(
        &self,
        function: LLVMValueRef,
        instructions: &[LLVMValueRef],
        state: &mut Homes,
    ) {
        unsafe {
            let mut by_block: Vec<(LLVMBasicBlockRef, Vec<LLVMValueRef>)> = Vec::new();
            for &instruction in instructions {
                if LLVMGetInstructionOpcode(instruction) != LLVMOpcode::LLVMPHI
                    || !self.is_large(LLVMTypeOf(instruction))
                {
                    continue;
                }
                let block = LLVMGetInstructionParent(instruction);
                match by_block.iter_mut().find(|(existing, _)| *existing == block) {
                    Some((_, phis)) => phis.push(instruction),
                    None => by_block.push((block, vec![instruction])),
                }
            }
            let pointer = LLVMPointerTypeInContext(self.context, 0);
            for (block, phis) in by_block {
                let mut chosen = Vec::with_capacity(phis.len());
                for &phi in &phis {
                    let count = LLVMCountIncoming(phi);
                    let mut values = Vec::with_capacity(count as usize);
                    let mut blocks = Vec::with_capacity(count as usize);
                    for index in 0..count {
                        let incoming = LLVMGetIncomingValue(phi, index);
                        values.push(self.home_of(function, incoming, state));
                        blocks.push(LLVMGetIncomingBlock(phi, index));
                    }
                    LLVMPositionBuilderBefore(self.builder, phi);
                    let address = LLVMBuildPhi(self.builder, pointer, c"aggregate.phi".as_ptr());
                    LLVMAddIncoming(address, values.as_mut_ptr(), blocks.as_mut_ptr(), count);
                    chosen.push(address);
                }
                let mut first = LLVMGetFirstInstruction(block);
                while LLVMGetInstructionOpcode(first) == LLVMOpcode::LLVMPHI {
                    first = LLVMGetNextInstruction(first);
                }
                let staged = if phis.len() > 1 {
                    let mut staged = Vec::with_capacity(phis.len());
                    for (&phi, &address) in phis.iter().zip(&chosen) {
                        let ty = LLVMTypeOf(phi);
                        let temporary = self.entry_alloca(function, ty);
                        LLVMPositionBuilderBefore(self.builder, first);
                        self.copy(ty, temporary, self.align_of(ty), address, self.align_of(ty));
                        staged.push(temporary);
                    }
                    staged
                } else {
                    chosen
                };
                for (&phi, &source) in phis.iter().zip(&staged) {
                    let ty = LLVMTypeOf(phi);
                    LLVMPositionBuilderBefore(self.builder, first);
                    self.copy(
                        ty,
                        state.homes[&phi],
                        self.align_of(ty),
                        source,
                        self.align_of(ty),
                    );
                    state.dead.push(phi);
                }
            }
        }
    }

    /// Erases replaced instructions. Any use left behind belongs to an
    /// instruction the pass does not rewrite, so it reloads from the home.
    unsafe fn erase(&self, state: &mut Homes) {
        unsafe {
            let mut replaced = std::mem::take(&mut state.replaced);
            replaced.extend(state.dead.iter().copied());
            let dead = state.dead.iter().copied().collect::<HashSet<_>>();
            for &value in &replaced {
                let ty = LLVMTypeOf(value);
                if LLVMGetTypeKind(ty) == LLVMTypeKind::LLVMVoidTypeKind {
                    continue;
                }
                let Some(&home) = state.homes.get(&value) else {
                    continue;
                };
                let mut use_ = LLVMGetFirstUse(value);
                while !use_.is_null() {
                    let next = LLVMGetNextUse(use_);
                    let user = LLVMGetUser(use_);
                    if !dead.contains(&user) && !LLVMIsAInstruction(user).is_null() {
                        LLVMPositionBuilderBefore(self.builder, user);
                        let load =
                            LLVMBuildLoad2(self.builder, ty, home, c"aggregate.reload".as_ptr());
                        LLVMSetAlignment(load, self.align_of(ty));
                        for index in 0..LLVMGetNumOperands(user) {
                            if LLVMGetOperand(user, index as c_uint) == value {
                                LLVMSetOperand(user, index as c_uint, load);
                            }
                        }
                    }
                    use_ = next;
                }
            }
            for &value in &replaced {
                let ty = LLVMTypeOf(value);
                if LLVMGetTypeKind(ty) != LLVMTypeKind::LLVMVoidTypeKind {
                    LLVMReplaceAllUsesWith(value, LLVMGetPoison(ty));
                }
            }
            for &instruction in state.dead.iter().rev() {
                LLVMInstructionEraseFromParent(instruction);
            }
        }
    }
}

unsafe fn instructions(function: LLVMValueRef) -> Vec<LLVMValueRef> {
    unsafe {
        let mut instructions = Vec::new();
        let mut block = LLVMGetFirstBasicBlock(function);
        while !block.is_null() {
            let mut instruction = LLVMGetFirstInstruction(block);
            while !instruction.is_null() {
                instructions.push(instruction);
                instruction = LLVMGetNextInstruction(instruction);
            }
            block = LLVMGetNextBasicBlock(block);
        }
        instructions
    }
}

/// Whether `load` reads a stack slot that nothing writes once the load has
/// run: the slot does not escape, and no instruction that writes it can
/// execute after the load. Such a slot can be the loaded value's home.
unsafe fn final_load_of_slot(load: LLVMValueRef, slot: LLVMValueRef) -> bool {
    unsafe {
        if LLVMIsAAllocaInst(slot).is_null() {
            return false;
        }
        // Every instruction that may write the slot, through any address
        // derived from it. An unknown use could write it at any time.
        let mut writes = Vec::new();
        let mut pending = vec![slot];
        while let Some(address) = pending.pop() {
            let mut use_ = LLVMGetFirstUse(address);
            while !use_.is_null() {
                let user = LLVMGetUser(use_);
                use_ = LLVMGetNextUse(use_);
                if LLVMIsAInstruction(user).is_null() {
                    return false;
                }
                match LLVMGetInstructionOpcode(user) {
                    LLVMOpcode::LLVMGetElementPtr => pending.push(user),
                    LLVMOpcode::LLVMLoad => {}
                    LLVMOpcode::LLVMStore
                        if LLVMGetOperand(user, 1) == address
                            && LLVMGetOperand(user, 0) != address =>
                    {
                        writes.push(user)
                    }
                    LLVMOpcode::LLVMCall => {
                        let callee = LLVMIsAFunction(LLVMGetCalledValue(user));
                        let name = if callee.is_null() {
                            String::new()
                        } else {
                            let mut length = 0;
                            let name = LLVMGetValueName2(callee, &mut length);
                            String::from_utf8_lossy(std::slice::from_raw_parts(
                                name.cast::<u8>(),
                                length,
                            ))
                            .into_owned()
                        };
                        let destination = LLVMGetOperand(user, 0) == address;
                        if name.starts_with("llvm.memcpy") || name.starts_with("llvm.memmove") {
                            if destination {
                                writes.push(user);
                            }
                        } else if name.starts_with("llvm.memset") && destination {
                            writes.push(user);
                        } else if !name.starts_with("llvm.lifetime") {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
        }
        // Blocks the load can reach, and whether its own block is in a cycle.
        let start = LLVMGetInstructionParent(load);
        let mut reachable = HashSet::new();
        let mut frontier = successors(start);
        while let Some(block) = frontier.pop() {
            if reachable.insert(block) {
                frontier.extend(successors(block));
            }
        }
        let after_load = |instruction: LLVMValueRef| {
            let mut cursor = LLVMGetNextInstruction(load);
            while !cursor.is_null() {
                if cursor == instruction {
                    return true;
                }
                cursor = LLVMGetNextInstruction(cursor);
            }
            false
        };
        writes.iter().all(|&write| {
            let block = LLVMGetInstructionParent(write);
            !reachable.contains(&block) && !(block == start && after_load(write))
        })
    }
}

unsafe fn successors(block: LLVMBasicBlockRef) -> Vec<LLVMBasicBlockRef> {
    unsafe {
        let terminator = LLVMGetBasicBlockTerminator(block);
        if terminator.is_null() {
            return Vec::new();
        }
        (0..LLVMGetNumSuccessors(terminator))
            .map(|index| LLVMGetSuccessor(terminator, index))
            .collect()
    }
}

unsafe fn has_single_use(value: LLVMValueRef) -> bool {
    unsafe {
        let first = LLVMGetFirstUse(value);
        !first.is_null() && LLVMGetNextUse(first).is_null()
    }
}

unsafe fn copy_function_attributes(from: LLVMValueRef, to: LLVMValueRef) {
    unsafe {
        let index = inkwell::llvm_sys::LLVMAttributeFunctionIndex;
        let count = LLVMGetAttributeCountAtIndex(from, index);
        let mut attributes = vec![std::ptr::null_mut(); count as usize];
        LLVMGetAttributesAtIndex(from, index, attributes.as_mut_ptr());
        for attribute in attributes {
            LLVMAddAttributeAtIndex(to, index, attribute);
        }
    }
}
